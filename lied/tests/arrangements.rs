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

// ---------------------------------------------------------------------------
// PATCH is a partial update (issue #23).
// ---------------------------------------------------------------------------

/// GET `uri` and return its ETag.
async fn etag_of(app: &axum::Router, token: &str, uri: &str) -> String {
    let request = bearer_request("GET", uri, token)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK, "GET {uri}");
    response.headers()[axum::http::header::ETAG]
        .to_str()
        .unwrap()
        .to_string()
}

/// PATCH `uri` with its current ETag; returns status, response ETag and body.
async fn patch_json(
    app: &axum::Router,
    token: &str,
    uri: &str,
    body: serde_json::Value,
) -> (axum::http::StatusCode, Option<String>, serde_json::Value) {
    let etag = etag_of(app, token, uri).await;
    let request = bearer_request("PATCH", uri, token)
        .header(axum::http::header::IF_MATCH, etag)
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let etag = response
        .headers()
        .get(axum::http::header::ETAG)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, etag, body)
}

#[tokio::test]
async fn arrangement_patch_changes_only_the_fields_it_sends() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({
                "title": "Carmen Suite",
                "arranger": "Hoffmann",
                "publisher": "Schott",
                "licenseNotes": "Purchased 2019",
                "difficulty": 5,
                "durationSeconds": 900,
                "status": "archived",
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let id = body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/orgs/{org_id}/arrangements/{id}");

    // A title-only PATCH keeps every provenance field (it used to null them).
    let (status, etag, body) = patch_json(
        &app,
        &token,
        &uri,
        serde_json::json!({ "title": "Carmen Suite No. 2" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(etag.is_some(), "a PATCH response carries the new ETag");
    assert_eq!(body["title"], "Carmen Suite No. 2");
    assert_eq!(body["arranger"], "Hoffmann");
    assert_eq!(body["publisher"], "Schott");
    assert_eq!(body["licenseNotes"], "Purchased 2019");
    assert_eq!(body["difficulty"], 5);
    assert_eq!(body["durationSeconds"], 900);
    assert_eq!(body["status"], "archived");

    // `null` clears a nullable field and leaves the rest alone.
    let (status, _, body) =
        patch_json(&app, &token, &uri, serde_json::json!({ "publisher": null })).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body["publisher"].is_null());
    assert_eq!(body["arranger"], "Hoffmann");
    assert_eq!(body["title"], "Carmen Suite No. 2");

    // A required field may be omitted but not blanked; `null` on it keeps it.
    let (status, _, _) = patch_json(&app, &token, &uri, serde_json::json!({ "title": " " })).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    let (status, _, body) =
        patch_json(&app, &token, &uri, serde_json::json!({ "title": null })).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["title"], "Carmen Suite No. 2");

    // A misspelled field is refused, not silently ignored.
    let (status, _, problem) = patch_json(
        &app,
        &token,
        &uri,
        serde_json::json!({ "publsher": "Bärenreiter" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(problem["errors"]["publsher"].is_array(), "{problem}");
    let (_, _, body) = patch_json(&app, &token, &uri, serde_json::json!({})).await;
    assert!(
        body["publisher"].is_null(),
        "the refused PATCH wrote nothing"
    );

    // A PATCH that changes nothing writes nothing: same ETag, no audit row.
    let before = etag_of(&app, &token, &uri).await;
    let audits = audit_count(&db.pool, "arrangement.update").await;
    let (status, etag, _) = patch_json(
        &app,
        &token,
        &uri,
        serde_json::json!({ "arranger": "Hoffmann" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(etag.as_deref(), Some(before.as_str()));
    let (status, etag, _) = patch_json(&app, &token, &uri, serde_json::json!({})).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(etag.as_deref(), Some(before.as_str()));
    assert_eq!(audit_count(&db.pool, "arrangement.update").await, audits);
}

#[tokio::test]
async fn work_patch_keeps_the_composer_unless_it_is_sent() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (_org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let request = bearer_request("POST", "/v1/works", &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "The Planets", "composer": "Holst" }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let id = body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/works/{id}");

    let (status, _, body) = patch_json(
        &app,
        &token,
        &uri,
        serde_json::json!({ "title": "The Planets, Op. 32" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["title"], "The Planets, Op. 32");
    assert_eq!(body["composer"], "Holst");

    let (status, _, body) =
        patch_json(&app, &token, &uri, serde_json::json!({ "composer": null })).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body["composer"].is_null());
    assert_eq!(body["title"], "The Planets, Op. 32");
}

#[tokio::test]
async fn voice_patch_changes_only_the_fields_it_sends() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;
    let instruments: Vec<Uuid> =
        sqlx::query_scalar!(r#"SELECT id FROM instrument ORDER BY key LIMIT 2"#)
            .fetch_all(&db.pool)
            .await
            .unwrap();

    let arr = create_arrangement_json(&app, &token, org_id, "Bolero").await;
    let arr_id = arr["id"].as_str().unwrap();
    let request = bearer_request(
        "POST",
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices"),
        &token,
    )
    .body(axum::body::Body::from(
        serde_json::json!({ "name": "Flute 2", "instrumentId": instruments[0] }).to_string(),
    ))
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let voice_id = body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}");

    // Instrument only: the name and the guessed part number stay.
    let (status, _, body) = patch_json(
        &app,
        &token,
        &uri,
        serde_json::json!({ "instrumentId": instruments[1] }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["name"], "Flute 2");
    assert_eq!(body["instrumentId"], instruments[1].to_string());
    assert_eq!(body["partNumber"], 2);

    // Name only: the instrument stays, and the part number is not re-guessed.
    let (status, _, body) =
        patch_json(&app, &token, &uri, serde_json::json!({ "name": "Piccolo" })).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["name"], "Piccolo");
    assert_eq!(body["instrumentId"], instruments[1].to_string());
    assert_eq!(body["partNumber"], 2);
}

#[tokio::test]
async fn tag_patch_keeps_the_kind_unless_it_is_sent() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/tags"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "name": "Festive", "kind": "occasion" }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let tag_id = body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/orgs/{org_id}/tags/{tag_id}");

    let (status, _, body) = patch_json(
        &app,
        &token,
        &uri,
        serde_json::json!({ "name": "Celebration" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["name"], "Celebration");
    assert_eq!(body["kind"], "occasion");

    let (status, _, body) =
        patch_json(&app, &token, &uri, serde_json::json!({ "kind": null })).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body["kind"].is_null());
    assert_eq!(body["name"], "Celebration");
}

#[tokio::test]
async fn arrangement_patch_validates_the_merged_row_and_audits_what_changed() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({
                "title": "Egmont",
                "difficulty": 6,
                "difficultyRatings": { "abrsm": "6" },
                "licenseNotes": "Hire only",
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let id = body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/orgs/{org_id}/arrangements/{id}");

    // Out-of-range values are a 400 naming the field, not a CHECK-violation 500.
    for (body, field) in [
        (serde_json::json!({ "difficulty": 9 }), "difficulty"),
        (serde_json::json!({ "status": "retired" }), "status"),
        // Fine on its own, but disagrees with the stored ABRSM rating.
        (serde_json::json!({ "difficulty": 3 }), "difficultyRatings"),
    ] {
        let (status, _, problem) = patch_json(&app, &token, &uri, body.clone()).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
        assert!(problem["errors"][field].is_array(), "{body} → {problem}");
    }
    // Changing both together is fine.
    let (status, _, _) = patch_json(
        &app,
        &token,
        &uri,
        serde_json::json!({ "difficulty": 3, "difficultyRatings": { "abrsm": "3" } }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // Creating with a mismatched pair is refused the same way.
    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Coriolan", "difficulty": 2,
                                "difficultyRatings": { "abrsm": "7" } })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);

    // The audit row keeps a value the PATCH erased.
    let (status, _, _) = patch_json(
        &app,
        &token,
        &uri,
        serde_json::json!({ "licenseNotes": null }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE action = 'arrangement.update' ORDER BY at DESC, id DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        payload,
        serde_json::json!({
            "before": { "licenseNotes": "Hire only" },
            "after": { "licenseNotes": null },
        })
    );
}

// ---------------------------------------------------------------------------
// Rejected request bodies are Problem Details (issue #62).
// ---------------------------------------------------------------------------

/// Send `body` raw with `content_type`; return status, the response's
/// request-id header and its Problem Details body.
async fn send_raw(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    if_match: Option<&str>,
    content_type: Option<&str>,
    body: impl Into<axum::body::Body>,
) -> (axum::http::StatusCode, String, serde_json::Value) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(etag) = if_match {
        builder = builder.header(axum::http::header::IF_MATCH, etag);
    }
    if let Some(content_type) = content_type {
        builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
    }
    let response = app
        .clone()
        .oneshot(builder.body(body.into()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(
        response.headers()[axum::http::header::CONTENT_TYPE],
        "application/problem+json",
        "{method} {uri} → {status}"
    );
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    (status, request_id, body_json(response).await)
}

#[tokio::test]
async fn rejected_bodies_are_problem_details_on_create_and_patch() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;
    let created = create_arrangement_json(&app, &token, org_id, "Bolero").await;
    let create_uri = format!("/v1/orgs/{org_id}/arrangements");
    let patch_uri = format!("{create_uri}/{}", created["id"].as_str().unwrap());
    let json = Some("application/json");

    for (method, uri) in [("POST", &create_uri), ("PATCH", &patch_uri)] {
        let etag = etag_of(&app, &token, &patch_uri).await;
        let if_match = (method == "PATCH").then_some(etag.as_str());

        // Shape errors: 400 validation-failed, keyed by the offending field.
        let mut cases = vec![
            (r#"{"title":"x","difficulty":"hard"}"#, "difficulty"),
            (r#"{"title":"x","publsher":"Schott"}"#, "publsher"),
        ];
        if method == "POST" {
            cases.push((r#"{"arranger":"Ravel"}"#, "title"));
        }
        for (body, field) in cases {
            let (status, request_id, problem) =
                send_raw(&app, &token, method, uri, if_match, json, body).await;
            assert_eq!(
                status,
                axum::http::StatusCode::BAD_REQUEST,
                "{method} {body}"
            );
            assert_eq!(problem["type"], "https://lied/errors/validation-failed");
            assert!(
                problem["errors"][field].is_array(),
                "{method} {body} → {problem}"
            );
            assert_eq!(problem["instance"], format!("urn:uuid:{request_id}"));
        }

        // Not JSON at all: 400 bad-request.
        for body in ["{", r#"{"title":"x",}"#, ""] {
            let (status, _, problem) =
                send_raw(&app, &token, method, uri, if_match, json, body).await;
            assert_eq!(
                status,
                axum::http::StatusCode::BAD_REQUEST,
                "{method} {body:?}"
            );
            assert_eq!(problem["type"], "https://lied/errors/bad-request");
        }

        // No JSON content type: 415.
        for content_type in [None, Some("text/plain")] {
            let (status, _, problem) = send_raw(
                &app,
                &token,
                method,
                uri,
                if_match,
                content_type,
                r#"{"title":"x"}"#,
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert_eq!(
                problem["type"],
                "https://lied/errors/unsupported-media-type"
            );
        }

        // Over LIED_MAX_REQUEST_BYTES (256 KiB in the test config): 413.
        let huge = serde_json::json!({ "title": "x".repeat(300 * 1024) }).to_string();
        let (status, _, problem) = send_raw(&app, &token, method, uri, if_match, json, huge).await;
        assert_eq!(status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(problem["type"], "https://lied/errors/payload-too-large");
    }

    // Nothing above wrote anything.
    let (status, _, body) = patch_json(&app, &token, &patch_uri, serde_json::json!({})).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["title"], "Bolero");
    assert_eq!(audit_count(&db.pool, "arrangement.update").await, 0);
}

#[tokio::test]
async fn a_non_uuid_request_id_is_replaced_once_and_used_everywhere() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;
    let uri = format!("/v1/orgs/{org_id}/arrangements");

    // A refused body: the Problem Details `instance` and the echoed header agree.
    let request = bearer_request("POST", &uri, &token)
        .header("x-request-id", "abc-123")
        .body(axum::body::Body::from(r#"{"arranger":"Ravel"}"#))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    let echoed = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    let id = Uuid::parse_str(&echoed).expect("the echoed id is a UUID");
    assert_eq!(
        body_json(response).await["instance"],
        format!("urn:uuid:{id}")
    );

    // A write: its audit row carries the same id the response echoes.
    let request = bearer_request("POST", &uri, &token)
        .header("x-request-id", "abc-123")
        .body(axum::body::Body::from(r#"{"title":"Bolero"}"#))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let echoed = Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
    let audited: Option<Uuid> =
        sqlx::query_scalar("SELECT request_id FROM audit_log WHERE action = 'arrangement.create'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(audited, Some(echoed));
}

// ---------------------------------------------------------------------------
// Guarded writes: the version check sits inside the statement (issue #64).
// ---------------------------------------------------------------------------

fn version(at: chrono::DateTime<chrono::Utc>) -> i64 {
    lied::listing::version_of(at)
}

/// Every guarded domain write refuses a stale version without writing, and
/// goes through at the current one.
#[tokio::test]
async fn every_guarded_write_refuses_a_stale_version() {
    use lied::domain::{collection, collection_item, part_assignment, tag, voice, Change};
    let db = TestDb::create_and_migrate().await;
    let pool = &db.pool;
    let org = create_org(pool, "Guarded").await;
    let player = create_plain_user(pool, "player").await;
    let instrument = first_instrument_id(pool).await;

    let fields = |title: &'static str| arrangement::ArrangementFields {
        title,
        work_id: None,
        instrumentation: None,
        arranger: None,
        publisher: None,
        purchase_date: None,
        license_notes: None,
        copy_count_allowed: None,
        status: "active",
        duration_seconds: None,
        difficulty: None,
        difficulty_ratings: None,
        difficulty_notes: None,
    };
    let arr = arrangement::create(
        pool,
        Uuid::now_v7(),
        org.id,
        "bolero",
        fields("Bolero"),
        None,
    )
    .await
    .unwrap();
    let stale = |at| Some(version(at) - 1);

    // Updates: stale → nothing written; current → written.
    assert!(
        arrangement::update(pool, arr.id, stale(arr.updated_at), fields("X"))
            .await
            .unwrap()
            .is_none()
    );
    let arr = arrangement::update(
        pool,
        arr.id,
        Some(version(arr.updated_at)),
        fields("Boléro"),
    )
    .await
    .unwrap()
    .expect("current version writes");
    assert_eq!(arr.title, "Boléro");

    let w = work::create(pool, Uuid::now_v7(), "Egmont", None, None)
        .await
        .unwrap();
    assert!(work::update(pool, w.id, stale(w.updated_at), "X", None)
        .await
        .unwrap()
        .is_none());
    let w = work::update(
        pool,
        w.id,
        Some(version(w.updated_at)),
        "Egmont, Op. 84",
        None,
    )
    .await
    .unwrap()
    .unwrap();

    let v = voice::create(
        pool,
        Uuid::now_v7(),
        arr.id,
        "Flute 1",
        "flute-1",
        instrument,
        voice::NewPartNumber::FromName,
        None,
    )
    .await
    .unwrap();
    assert!(voice::update(
        pool,
        v.id,
        stale(v.updated_at),
        "X",
        instrument,
        Change::Keep
    )
    .await
    .unwrap()
    .is_none());
    let v = voice::update(
        pool,
        v.id,
        Some(version(v.updated_at)),
        "Flute I",
        instrument,
        Change::Keep,
    )
    .await
    .unwrap()
    .unwrap();

    let t = tag::create(pool, Uuid::now_v7(), org.id, "Festive", None, None)
        .await
        .unwrap();
    assert!(tag::update(pool, t.id, stale(t.updated_at), "X", None)
        .await
        .unwrap()
        .is_none());
    let t = tag::update(pool, t.id, Some(version(t.updated_at)), "Feast", None)
        .await
        .unwrap()
        .unwrap();

    let c = collection::create(
        pool,
        Uuid::now_v7(),
        org.id,
        "Spring",
        "spring",
        "program",
        None,
    )
    .await
    .unwrap();
    assert!(
        collection::update(pool, c.id, stale(c.updated_at), "X", "program")
            .await
            .unwrap()
            .is_none()
    );
    let c = collection::update(
        pool,
        c.id,
        Some(version(c.updated_at)),
        "Spring Gala",
        "program",
    )
    .await
    .unwrap()
    .unwrap();

    let item = collection_item::create(pool, Uuid::now_v7(), c.id, arr.id, 1, None)
        .await
        .unwrap();
    assert!(
        collection_item::update_index(pool, item.id, stale(item.updated_at), 1)
            .await
            .unwrap()
            .is_none()
    );
    let item = collection_item::update_index(pool, item.id, Some(version(item.updated_at)), 1)
        .await
        .unwrap()
        .unwrap();

    let pa = part_assignment::assign(pool, Uuid::now_v7(), item.id, v.id, player.id, None)
        .await
        .unwrap();
    let now = Change::Set(Some(chrono::Utc::now()));
    assert!(
        part_assignment::update_state(pool, pa.id, stale(pa.updated_at), now, Change::Keep)
            .await
            .unwrap()
            .is_none()
    );
    let pa =
        part_assignment::update_state(pool, pa.id, Some(version(pa.updated_at)), now, Change::Keep)
            .await
            .unwrap()
            .unwrap();

    let m = membership::create(
        pool,
        Uuid::now_v7(),
        player.id,
        org.id,
        membership::MembershipFields {
            role: membership::Role::Musician,
            instrument_ids: &[],
            is_principal: false,
            principal_instrument_ids: &[],
        },
        None,
    )
    .await
    .unwrap();
    let conductor = membership::MembershipFields {
        role: membership::Role::Conductor,
        instrument_ids: &[],
        is_principal: false,
        principal_instrument_ids: &[],
    };
    assert!(
        membership::update(pool, m.id, stale(m.updated_at), conductor)
            .await
            .unwrap()
            .is_none()
    );
    let conductor = membership::MembershipFields {
        role: membership::Role::Conductor,
        instrument_ids: &[],
        is_principal: false,
        principal_instrument_ids: &[],
    };
    let m = membership::update(pool, m.id, Some(version(m.updated_at)), conductor)
        .await
        .unwrap()
        .unwrap();

    assert!(
        organization::update_name(pool, org.id, stale(org.updated_at), "X")
            .await
            .unwrap()
            .is_none()
    );
    let org = organization::update_name(pool, org.id, Some(version(org.updated_at)), "Guarded Org")
        .await
        .unwrap()
        .unwrap();

    // Deletes, leaves first: stale → still there; current → gone.
    assert!(!part_assignment::delete(pool, pa.id, stale(pa.updated_at))
        .await
        .unwrap());
    assert!(
        part_assignment::delete(pool, pa.id, Some(version(pa.updated_at)))
            .await
            .unwrap()
    );
    assert!(
        !collection_item::remove(pool, c.id, item.id, stale(item.updated_at))
            .await
            .unwrap()
    );
    assert!(
        collection_item::remove(pool, c.id, item.id, Some(version(item.updated_at)))
            .await
            .unwrap()
    );
    assert!(!collection::soft_delete(pool, c.id, stale(c.updated_at))
        .await
        .unwrap());
    assert!(
        collection::soft_delete(pool, c.id, Some(version(c.updated_at)))
            .await
            .unwrap()
    );
    assert!(!voice::soft_delete(pool, v.id, stale(v.updated_at))
        .await
        .unwrap());
    assert!(voice::soft_delete(pool, v.id, Some(version(v.updated_at)))
        .await
        .unwrap());
    assert!(!tag::soft_delete(pool, t.id, stale(t.updated_at))
        .await
        .unwrap());
    assert!(tag::soft_delete(pool, t.id, Some(version(t.updated_at)))
        .await
        .unwrap());
    assert!(
        !arrangement::soft_delete(pool, arr.id, stale(arr.updated_at))
            .await
            .unwrap()
    );
    assert!(
        arrangement::soft_delete(pool, arr.id, Some(version(arr.updated_at)))
            .await
            .unwrap()
    );
    assert!(!membership::delete(pool, m.id, stale(m.updated_at))
        .await
        .unwrap());
    assert!(membership::delete(pool, m.id, Some(version(m.updated_at)))
        .await
        .unwrap());
    assert!(!work::delete(pool, w.id, stale(w.updated_at)).await.unwrap());
    assert!(work::delete(pool, w.id, Some(version(w.updated_at)))
        .await
        .unwrap());
    // The org delete is a cascade: a stale one must remove nothing at all.
    let survivor = arrangement::create(
        pool,
        Uuid::now_v7(),
        org.id,
        "egmont",
        fields("Egmont"),
        None,
    )
    .await
    .unwrap();
    assert!(!organization::delete(pool, org.id, stale(org.updated_at))
        .await
        .unwrap());
    assert!(arrangement::find_by_id(pool, survivor.id)
        .await
        .unwrap()
        .is_some());
    assert!(
        organization::delete(pool, org.id, Some(version(org.updated_at)))
            .await
            .unwrap()
    );
    let loner = create_plain_user(pool, "loner").await;
    assert!(!user::delete(pool, loner.id, stale(loner.updated_at))
        .await
        .unwrap());
    assert!(
        user::delete(pool, loner.id, Some(version(loner.updated_at)))
            .await
            .unwrap()
    );
}

/// Concurrent PATCHes holding one ETag: exactly one wins, the others get
/// 412, and the winner's change survives (before #64 every one passed the
/// read-time check and the last write erased the others).
#[tokio::test]
async fn concurrent_writes_with_one_etag_let_exactly_one_through() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;
    let created = create_arrangement_json(&app, &token, org_id, "Bolero").await;
    let uri = format!(
        "/v1/orgs/{org_id}/arrangements/{}",
        created["id"].as_str().unwrap()
    );
    let etag = etag_of(&app, &token, &uri).await;

    let attempts = 8;
    let sends = (0..attempts).map(|n| {
        let request = bearer_request("PATCH", &uri, &token)
            .header(axum::http::header::IF_MATCH, &etag)
            .body(axum::body::Body::from(
                serde_json::json!({ "publisher": format!("Publisher {n}") }).to_string(),
            ))
            .unwrap();
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            (status, body_json(response).await)
        }
    });
    let results = futures_util::future::join_all(sends).await;

    let winners: Vec<&serde_json::Value> = results
        .iter()
        .filter(|(status, _)| *status == axum::http::StatusCode::OK)
        .map(|(_, body)| body)
        .collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert!(results
        .iter()
        .all(|(status, _)| *status == axum::http::StatusCode::OK
            || *status == axum::http::StatusCode::PRECONDITION_FAILED));
    let request = bearer_request("GET", &uri, &token)
        .body(axum::body::Body::empty())
        .unwrap();
    let now = body_json(app.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(
        now["publisher"], winners[0]["publisher"],
        "the winner's change survives"
    );
    assert_eq!(audit_count(&db.pool, "arrangement.update").await, 1);

    // Same for deletes: one 204, the rest 412 (stale) or 404 (already gone).
    let etag = etag_of(&app, &token, &uri).await;
    let deletes = (0..attempts).map(|_| {
        let request = bearer_request("DELETE", &uri, &token)
            .header(axum::http::header::IF_MATCH, &etag)
            .body(axum::body::Body::empty())
            .unwrap();
        let app = app.clone();
        async move { app.oneshot(request).await.unwrap().status() }
    });
    let statuses = futures_util::future::join_all(deletes).await;
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == axum::http::StatusCode::NO_CONTENT)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(audit_count(&db.pool, "arrangement.soft_delete").await, 1);
}

/// A work delete that is refused leaves no audit row (it was written before
/// the delete, so a 409 claimed a deletion that never happened).
#[tokio::test]
async fn a_refused_work_delete_is_not_audited() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;

    let request = bearer_request("POST", "/v1/works", &token)
        .body(axum::body::Body::from(r#"{"title":"Egmont"}"#))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let work_id = body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let request = bearer_request("POST", &format!("/v1/orgs/{org_id}/arrangements"), &token)
        .body(axum::body::Body::from(
            serde_json::json!({ "title": "Egmont Overture", "workId": work_id }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        axum::http::StatusCode::CREATED
    );

    let uri = format!("/v1/works/{work_id}");
    let etag = etag_of(&app, &token, &uri).await;
    let request = bearer_request("DELETE", &uri, &token)
        .header(axum::http::header::IF_MATCH, etag)
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        axum::http::StatusCode::CONFLICT
    );
    assert_eq!(audit_count(&db.pool, "work.delete").await, 0);
}

// ---------------------------------------------------------------------------
// Rejected path and query parameters are Problem Details (issue #66).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bad_path_and_query_parameters_are_problem_details_naming_them() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) =
        org_with_member(&db.pool, &state, "Org", "arch", membership::Role::Archivist).await;
    let created = create_arrangement_json(&app, &token, org_id, "Bolero").await;
    let arr_id = created["id"].as_str().unwrap();
    let coll = Uuid::now_v7();

    for (uri, field) in [
        // A single path parameter.
        ("/v1/works/not-a-uuid".to_string(), "id"),
        // Tuple paths: first and second position, both named.
        ("/v1/orgs/not-a-uuid/arrangements".to_string(), "orgId"),
        (format!("/v1/orgs/{org_id}/arrangements/nope"), "id"),
        (
            format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices/x1"),
            "id",
        ),
        // Query parameters: wrong type, out of range, missing.
        (format!("/v1/orgs/{org_id}/arrangements?limit=abc"), "limit"),
        (
            format!("/v1/orgs/{org_id}/arrangements?offset=-1"),
            "offset",
        ),
        (
            format!(
                "/v1/orgs/{org_id}/collections/{coll}/bulk-assignments/preview?instrumentId={}",
                Uuid::now_v7()
            ),
            "userId",
        ),
    ] {
        let (status, request_id, problem) = send_raw(
            &app,
            &token,
            "GET",
            &uri,
            None,
            None,
            axum::body::Body::empty(),
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::BAD_REQUEST,
            "{uri} → {problem}"
        );
        assert_eq!(
            problem["type"], "https://lied/errors/validation-failed",
            "{uri}"
        );
        assert!(problem["errors"][field].is_array(), "{uri} → {problem}");
        assert_eq!(
            problem["instance"],
            format!("urn:uuid:{request_id}"),
            "{uri}"
        );
    }

    // Well-formed parameters still work, and unknown query keys (the
    // `filter[…]` syntax) are not refused by the extractor.
    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/arrangements?limit=5&filter%5Bstatus%5D=active"),
        &token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        axum::http::StatusCode::OK
    );
}
