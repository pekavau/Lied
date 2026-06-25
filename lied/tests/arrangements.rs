//! Integration tests for issue #6 (Arrangement metadata management: Works,
//! Arrangements, Voices, Tags — the catalog backbone). Exercises the real
//! axum router end-to-end against an ephemeral Postgres per test
//! (`testcontainers`), driving requests with `tower::ServiceExt::oneshot`.
//!
//! Coverage maps to the issue's acceptance criteria:
//!   - Full CRUD for Work/Arrangement/Voice/Tag with the correct permission
//!     gates (owner/archivist write; musician read; conductor cannot upload
//!     arrangements per the permission matrix).
//!   - Slugs generated at creation and unique per org (duplicate -> 409).
//!   - Soft-delete hides the row from list/search; undelete restores it.
//!   - ILIKE `?q=` search matches on `Arrangement.title` and (via the Work
//!     join) `Work.composer`.
//!   - Work edit blocked for a non-creator non-admin (403).
//!   - Optimistic concurrency: stale/missing `If-Match` -> 412.
//!   - Every write produces an audit-log row.
//!
//! Follows the exact fixture/test-app pattern established in `tests/orgs.rs`.

use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{arrangement, membership, organization, user, work};
use lied::state::AppState;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

struct TestDb {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDb {
    async fn create_and_migrate() -> Self {
        let container = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("failed to start postgres container");

        let host = container.get_host().await.expect("container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("container port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("failed to connect to container postgres");

        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        Self {
            _container: container,
            pool,
        }
    }
}

fn test_config() -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from(
            "test-signing-key-material-for-arrangement-tests".to_string(),
        ),
        jwt_lifetime_days: 30,
        max_upload_bytes: 1,
        max_request_bytes: 256 * 1024,
        max_files_per_voice: 1,
        max_arrangements_per_org: None,
        max_page_size: 200,
        default_page_size: 50,
        ratelimit_auth_per_min: 120,
        ratelimit_anon_per_min: 20,
        ratelimit_login_per_min: 10,
        metrics_enabled: false,
        docs_enabled: true,
        tracing_pretty: false,
        secure_cookies: false,
    }
}

fn test_state(pool: PgPool, config: AppConfig) -> AppState {
    let jwt_keyring = Arc::new(auth::jwt::Keyring::from_single_key(
        config.jwt_signing_key.expose(),
        config.jwt_lifetime_days,
    ));

    let s3_config = aws_sdk_s3::config::Builder::new()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(config.s3_region.clone()))
        .endpoint_url(&config.s3_endpoint)
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            config.s3_access_key_id.expose(),
            config.s3_secret_access_key.expose(),
            None,
            None,
            "lied-test",
        ))
        .force_path_style(true)
        .build();

    AppState {
        config: Arc::new(config),
        db: pool,
        s3: aws_sdk_s3::Client::from_conf(s3_config),
        metrics: None,
        jwt_keyring,
    }
}

async fn build_test_app(pool: PgPool) -> (axum::Router, AppState) {
    let config = test_config();
    let state = test_state(pool, config);
    (lied::routes::build_router(state.clone()), state)
}

async fn create_plain_user(pool: &PgPool, username: &str) -> user::User {
    let hash = auth::password::hash_password("correct horse battery staple").expect("hash");
    let id = Uuid::now_v7();
    let slug = user::slugify(username);
    user::create(
        pool,
        id,
        &slug,
        username,
        None,
        Some(&hash),
        username,
        false,
        None,
    )
    .await
    .expect("create user")
}

async fn create_org(pool: &PgPool, name: &str) -> organization::Organization {
    let id = Uuid::now_v7();
    let slug = organization::slugify(name);
    organization::create(pool, id, name, &slug, None)
        .await
        .expect("create org")
}

async fn add_member(pool: &PgPool, org_id: Uuid, user_id: Uuid, role: membership::Role) {
    let id = Uuid::now_v7();
    membership::create(
        pool,
        id,
        user_id,
        org_id,
        membership::MembershipFields {
            role,
            instrument_ids: &[],
            is_principal: false,
            principal_instrument_ids: &[],
        },
        None,
    )
    .await
    .expect("create membership");
}

/// A user holding `role` in a freshly created org. Returns `(org_id, token)`.
async fn org_with_member(
    pool: &PgPool,
    state: &AppState,
    org_name: &str,
    username: &str,
    role: membership::Role,
) -> (Uuid, String) {
    let org = create_org(pool, org_name).await;
    let u = create_plain_user(pool, username).await;
    add_member(pool, org.id, u.id, role).await;
    (org.id, mint_token(state, u.id))
}

async fn first_instrument_id(pool: &PgPool) -> Uuid {
    sqlx::query_scalar!(r#"SELECT id FROM instrument ORDER BY key LIMIT 1"#)
        .fetch_one(pool)
        .await
        .expect("instrument seed present")
}

fn mint_token(state: &AppState, user_id: Uuid) -> String {
    let (token, _claims) = state.jwt_keyring.mint(user_id, None).expect("mint token");
    token
}

fn bearer_request(method: &str, uri: &str, token: &str) -> axum::http::request::Builder {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn audit_count(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar(r#"SELECT count(*) FROM audit_log WHERE action = $1"#)
        .bind(action)
        .fetch_one(pool)
        .await
        .expect("count audit rows")
}

/// POST an arrangement and return the created JSON body.
async fn create_arrangement_json(
    app: &axum::Router,
    token: &str,
    org_id: Uuid,
    title: &str,
) -> serde_json::Value {
    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": title }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    body_json(response).await
}

// ---------------------------------------------------------------------------
// Arrangement CRUD + permissions + slug + audit.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn archivist_creates_an_arrangement_with_a_slug_and_it_is_audited() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Strauss Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;

    let body = create_arrangement_json(&app, &token, org_id, "Radetzky March").await;

    assert_eq!(body["title"], "Radetzky March");
    assert_eq!(body["slug"], "radetzky-march");
    assert_eq!(body["status"], "active");
    assert_eq!(body["organizationId"], org_id.to_string());
    assert_eq!(audit_count(&db.pool, "arrangement.create").await, 1);
}

#[tokio::test]
async fn musician_cannot_create_an_arrangement() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "mus", membership::Role::Musician).await;

    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Forbidden" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn conductor_cannot_create_an_arrangement() {
    // Permission matrix: only owner/archivist "Upload/edit arrangements".
    // Conductor and archivist are incomparable, so a conductor fails the
    // archivist gate.
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "cond", membership::Role::Conductor).await;

    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Nope" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn duplicate_title_slug_in_one_org_is_rejected_with_409() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    create_arrangement_json(&app, &token, org_id, "Bolero").await;

    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Bolero" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// Soft-delete / undelete + ILIKE search.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn soft_delete_hides_from_list_and_search_then_undelete_restores() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let created = create_arrangement_json(&app, &token, org_id, "Hungarian Dance").await;
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let list_total = |app: axum::Router, token: String, uri: String| async move {
        let request = bearer_request("GET", &uri, &token)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        body_json(response).await["total"].as_i64().unwrap()
    };

    let list_uri = format!("/v1/orgs/{org_id}/arrangements");
    let search_uri = format!("/v1/orgs/{org_id}/arrangements?q=Hungarian");

    assert_eq!(
        list_total(app.clone(), token.clone(), list_uri.clone()).await,
        1
    );
    assert_eq!(
        list_total(app.clone(), token.clone(), search_uri.clone()).await,
        1
    );

    // Soft-delete requires a matching If-Match.
    let current = arrangement::find_by_id(&db.pool, id)
        .await
        .unwrap()
        .unwrap();
    let etag = lied::listing::etag_for(current.updated_at);
    let request = bearer_request(
        "DELETE",
        &format!("/v1/orgs/{org_id}/arrangements/{id}"),
        &token,
    )
    .header(axum::http::header::IF_MATCH, etag)
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);

    assert_eq!(
        list_total(app.clone(), token.clone(), list_uri.clone()).await,
        0
    );
    assert_eq!(
        list_total(app.clone(), token.clone(), search_uri.clone()).await,
        0
    );

    // Undelete restores.
    let request = bearer_request(
        "POST",
        &format!("/v1/orgs/{org_id}/arrangements/{id}/undelete"),
        &token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);

    assert_eq!(list_total(app.clone(), token.clone(), list_uri).await, 1);
    assert_eq!(audit_count(&db.pool, "arrangement.soft_delete").await, 1);
    assert_eq!(audit_count(&db.pool, "arrangement.undelete").await, 1);
}

#[tokio::test]
async fn search_matches_on_composer_via_the_work_join() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    // A Work carrying the composer, then an arrangement under it whose title
    // does NOT mention the composer.
    let w = work::create(
        &db.pool,
        Uuid::now_v7(),
        "Symphony No. 5",
        Some("Beethoven"),
        None,
    )
    .await
    .unwrap();

    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Fate Knocks", "workId": w.id }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);

    let total_for = |q: &str| {
        let app = app.clone();
        let token = token.clone();
        let uri = format!("/v1/orgs/{org_id}/arrangements?q={q}");
        async move {
            let request = bearer_request("GET", &uri, &token)
                .body(axum::body::Body::empty())
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            body_json(response).await["total"].as_i64().unwrap()
        }
    };

    assert_eq!(
        total_for("Beethoven").await,
        1,
        "composer match via work join"
    );
    assert_eq!(total_for("Fate").await, 1, "title match");
    assert_eq!(total_for("Mozart").await, 0, "no match");
}

// ---------------------------------------------------------------------------
// Optimistic concurrency.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_with_stale_or_missing_if_match_returns_412() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let created = create_arrangement_json(&app, &token, org_id, "Carmen Suite").await;
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let uri = format!("/v1/orgs/{org_id}/arrangements/{id}");

    // Missing If-Match -> 412.
    let request = bearer_request("PATCH", &uri, &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Carmen Suite No. 2" }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );

    // Stale If-Match -> 412.
    let request = bearer_request("PATCH", &uri, &token)
        .header(axum::http::header::IF_MATCH, "\"1\"")
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Carmen Suite No. 2" }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );

    // Correct If-Match -> 200.
    let current = arrangement::find_by_id(&db.pool, id)
        .await
        .unwrap()
        .unwrap();
    let etag = lied::listing::etag_for(current.updated_at);
    let request = bearer_request("PATCH", &uri, &token)
        .header(axum::http::header::IF_MATCH, etag)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Carmen Suite No. 2" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Work: open creation, creator-only edit.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn any_member_creates_a_work_but_a_non_creator_cannot_edit_it() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;

    // A musician (in some org) is "a member", so may create a Work.
    let (_org_id, creator_token) = org_with_member(
        &db.pool,
        &state,
        "Org",
        "creator",
        membership::Role::Musician,
    )
    .await;

    let request = bearer_request("POST", "/v1/works", &creator_token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "The Planets", "composer": "Holst" }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let work_body = body_json(response).await;
    let work_id = Uuid::parse_str(work_body["id"].as_str().unwrap()).unwrap();

    // A different member tries to edit it -> 403.
    let (_org2, other_token) =
        org_with_member(&db.pool, &state, "Org2", "other", membership::Role::Owner).await;
    let current = work::find_by_id(&db.pool, work_id).await.unwrap().unwrap();
    let etag = lied::listing::etag_for(current.updated_at);
    let request = bearer_request("PATCH", &format!("/v1/works/{work_id}"), &other_token)
        .header(axum::http::header::IF_MATCH, etag.clone())
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Hijacked" }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);

    // The creator can edit it.
    let request = bearer_request("PATCH", &format!("/v1/works/{work_id}"), &creator_token)
        .header(axum::http::header::IF_MATCH, etag)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "The Planets, Op. 32", "composer": "Holst" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

#[tokio::test]
async fn creating_a_work_without_any_membership_is_forbidden() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;

    // A user with no membership in any org.
    let loner = create_plain_user(&db.pool, "loner").await;
    let token = mint_token(&state, loner.id);

    let request = bearer_request("POST", "/v1/works", &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Orphan Work" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Voice CRUD.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn archivist_adds_a_voice_with_an_instrument_and_lists_it() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let arr = create_arrangement_json(&app, &token, org_id, "Overture").await;
    let arr_id = Uuid::parse_str(arr["id"].as_str().unwrap()).unwrap();
    let instrument_id = first_instrument_id(&db.pool).await;

    let request = bearer_request(
        "POST",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices"),
        &token,
    )
    .body(axum::body::Body::from(
        serde_json::json!({ "name": "Flute 1", "instrumentId": instrument_id }).to_string(),
    ))
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let voice = body_json(response).await;
    assert_eq!(voice["name"], "Flute 1");
    assert_eq!(voice["slug"], "flute-1");

    // List shows it.
    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices"),
        &token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(body_json(response).await["total"].as_i64().unwrap(), 1);
    assert_eq!(audit_count(&db.pool, "voice.create").await, 1);
}

#[tokio::test]
async fn creating_a_voice_with_an_unknown_instrument_returns_400() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let arr = create_arrangement_json(&app, &token, org_id, "Overture").await;
    let arr_id = Uuid::parse_str(arr["id"].as_str().unwrap()).unwrap();

    let request = bearer_request(
        "POST",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices"),
        &token,
    )
    .body(axum::body::Body::from(
        serde_json::json!({ "name": "Ghost", "instrumentId": Uuid::now_v7() }).to_string(),
    ))
    .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Tag CRUD + attach/detach.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn archivist_creates_a_tag_attaches_lists_and_detaches_it() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let arr = create_arrangement_json(&app, &token, org_id, "Festive Overture").await;
    let arr_id = Uuid::parse_str(arr["id"].as_str().unwrap()).unwrap();

    // Create a tag.
    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/tags"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "name": "Festive", "kind": "occasion" }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let tag = body_json(response).await;
    let tag_id = Uuid::parse_str(tag["id"].as_str().unwrap()).unwrap();

    // Attach it.
    let request = bearer_request(
        "POST",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/tags"),
        &token,
    )
    .body(axum::body::Body::from(
        serde_json::json!({ "tagId": tag_id }).to_string(),
    ))
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);

    // Duplicate attach -> 409.
    let request = bearer_request(
        "POST",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/tags"),
        &token,
    )
    .body(axum::body::Body::from(
        serde_json::json!({ "tagId": tag_id }).to_string(),
    ))
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);

    // List the arrangement's tags.
    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/tags"),
        &token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let tags = body_json(response).await;
    assert_eq!(tags.as_array().unwrap().len(), 1);

    // Detach it.
    let request = bearer_request(
        "DELETE",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/tags/{tag_id}"),
        &token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);

    assert_eq!(audit_count(&db.pool, "tag.create").await, 1);
    assert_eq!(audit_count(&db.pool, "arrangement_tag.create").await, 1);
    assert_eq!(audit_count(&db.pool, "arrangement_tag.delete").await, 1);
}

// ---------------------------------------------------------------------------
// Cross-org scoping.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_arrangement_is_not_reachable_through_another_orgs_path() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_a, token_a) = org_with_member(
        &db.pool,
        &state,
        "Org A",
        "arch_a",
        membership::Role::Archivist,
    )
    .await;

    let arr = create_arrangement_json(&app, &token_a, org_a, "Private Piece").await;
    let arr_id = Uuid::parse_str(arr["id"].as_str().unwrap()).unwrap();

    // A second org whose archivist tries to read org A's arrangement by id
    // through org B's path -> 404 (scoping), not 200.
    let (org_b, token_b) = org_with_member(
        &db.pool,
        &state,
        "Org B",
        "arch_b",
        membership::Role::Archivist,
    )
    .await;
    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_b}/arrangements/{arr_id}"),
        &token_b,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_voice_is_not_reachable_or_modifiable_through_another_orgs_path() {
    // Regression: a Voice has no organization_id of its own; org ownership is
    // checked through the parent arrangement. An archivist of org B (a real
    // role, so the role gate passes) must still not read/modify org A's voice
    // by id through org B's path.
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_a, token_a) = org_with_member(
        &db.pool,
        &state,
        "Org A",
        "arch_a",
        membership::Role::Archivist,
    )
    .await;

    let arr = create_arrangement_json(&app, &token_a, org_a, "Org A Piece").await;
    let arr_id = Uuid::parse_str(arr["id"].as_str().unwrap()).unwrap();
    let instrument_id = first_instrument_id(&db.pool).await;
    let v = bearer_request(
        "POST",
        &format!("/v1/orgs/{org_a}/arrangements/{arr_id}/voices"),
        &token_a,
    )
    .body(axum::body::Body::from(
        serde_json::json!({ "name": "Oboe", "instrumentId": instrument_id }).to_string(),
    ))
    .unwrap();
    let v = app.clone().oneshot(v).await.unwrap();
    assert_eq!(v.status(), axum::http::StatusCode::CREATED);
    let voice_id = Uuid::parse_str(body_json(v).await["id"].as_str().unwrap()).unwrap();

    let (org_b, token_b) = org_with_member(
        &db.pool,
        &state,
        "Org B",
        "arch_b",
        membership::Role::Archivist,
    )
    .await;
    let base = format!("/v1/orgs/{org_b}/arrangements/{arr_id}/voices/{voice_id}");

    // GET, PATCH, DELETE through org B's path all 404 (scoping precedes
    // concurrency, so PATCH/DELETE 404 even without a valid If-Match).
    for (method, body) in [
        ("GET", None),
        (
            "PATCH",
            Some(serde_json::json!({ "name": "Hijacked", "instrumentId": instrument_id })),
        ),
        ("DELETE", None),
    ] {
        let req = bearer_request(method, &base, &token_b);
        let req = match body {
            Some(b) => req.body(axum::body::Body::from(b.to_string())).unwrap(),
            None => req.body(axum::body::Body::empty()).unwrap(),
        };
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "{method} should 404 cross-org"
        );
    }
}

#[tokio::test]
async fn voices_can_be_sorted_by_created_at() {
    // Regression: the voice list joins `arrangement`, so an unqualified
    // `ORDER BY created_at` is ambiguous and 500s. `?sort=createdAt` must work.
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let arr = create_arrangement_json(&app, &token, org_id, "Symphonic Suite").await;
    let arr_id = Uuid::parse_str(arr["id"].as_str().unwrap()).unwrap();
    let instrument_id = first_instrument_id(&db.pool).await;

    for name in ["Clarinet 1", "Clarinet 2"] {
        let req = bearer_request(
            "POST",
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices"),
            &token,
        )
        .body(axum::body::Body::from(
            serde_json::json!({ "name": name, "instrumentId": instrument_id }).to_string(),
        ))
        .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            axum::http::StatusCode::CREATED
        );
    }

    let req = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices?sort=createdAt:desc"),
        &token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(body_json(response).await["total"].as_i64().unwrap(), 2);
}

#[tokio::test]
async fn an_invalid_instrument_id_filter_returns_400() {
    // Regression: a malformed filter[instrumentId] must 400, not silently
    // drop the filter and return all voices.
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let arr = create_arrangement_json(&app, &token, org_id, "Suite").await;
    let arr_id = Uuid::parse_str(arr["id"].as_str().unwrap()).unwrap();

    let req = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices?filter[instrumentId]=not-a-uuid"),
        &token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}
