//! Integration tests for issue #5 (Org/User/Membership management):
//! system-admin-only org/user provisioning, owner-managed memberships,
//! authorization gating (musician -> 403 on a manage-members route),
//! the last-owner invariant, pagination/sort/filter incl. 400 on an
//! unknown field, ETag/`If-Match` optimistic concurrency, and that every
//! write produces an audit-log row.
//!
//! Follows the exact fixture/test-app pattern established in `tests/auth.rs`
//! (issue #4): a fresh ephemeral Postgres per test via `testcontainers`, the
//! real axum router via `lied::routes::build_router`, requests driven with
//! `tower::ServiceExt::oneshot`.

use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{membership, organization, user};
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
        jwt_signing_key: Secret::from("test-signing-key-material-for-org-tests".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes: 1,
        max_request_bytes: 1,
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

async fn bootstrap_admin(pool: &PgPool, username: &str, password: &str) -> user::User {
    auth::admin::create_admin(
        pool,
        auth::admin::CreateAdminParams {
            username,
            email: None,
            display_name: "Test Admin",
            password,
        },
    )
    .await
    .expect("create_admin should succeed")
}

/// Create a plain (non-admin) user directly via the domain layer, bypassing
/// the `/v1` system-admin-gated endpoint — needed to set up musician/owner
/// fixtures without depending on the very endpoint under test.
async fn create_plain_user(pool: &PgPool, username: &str, password: &str) -> user::User {
    let hash = auth::password::hash_password(password).expect("hash password");
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

async fn add_member(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    role: membership::Role,
) -> membership::Membership {
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
    .expect("create membership")
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
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

// ---------------------------------------------------------------------------
// System-admin-only provisioning: orgs and users.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn system_admin_can_create_an_organization() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("POST", "/v1/orgs", &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(r#"{"name":"Vienna Philharmonic"}"#))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);

    let json = body_json(response).await;
    assert_eq!(json["name"], "Vienna Philharmonic");
    assert_eq!(json["slug"], "vienna-philharmonic");

    let audit_count: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) FROM audit_log WHERE action = 'organization.create'"#
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .unwrap_or(0);
    assert_eq!(audit_count, 1, "org creation should be audited");
}

#[tokio::test]
async fn non_admin_cannot_create_an_organization() {
    let db = TestDb::create_and_migrate().await;
    let plain = create_plain_user(&db.pool, "alice", "correct horse battery staple").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, plain.id);

    let request = bearer_request("POST", "/v1/orgs", &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(r#"{"name":"Some Org"}"#))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn system_admin_can_create_a_user() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("POST", "/v1/users", &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            r#"{"username":"newbie","displayName":"New Bie","password":"correct horse battery staple"}"#,
        ))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);

    let audit_count: i64 =
        sqlx::query_scalar!(r#"SELECT count(*) FROM audit_log WHERE action = 'user.create'"#)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .unwrap_or(0);
    assert_eq!(audit_count, 1, "user creation should be audited");
}

#[tokio::test]
async fn non_admin_cannot_create_or_list_users() {
    let db = TestDb::create_and_migrate().await;
    let plain = create_plain_user(&db.pool, "bob", "correct horse battery staple").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, plain.id);

    let request = bearer_request("GET", "/v1/users", &token)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn system_admin_can_delete_an_organization_with_if_match() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Disbandment Orchestra").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let etag = lied::listing::etag_for(org.updated_at);

    let request = bearer_request("DELETE", &format!("/v1/orgs/{}", org.id), &token)
        .header(axum::http::header::IF_MATCH, etag)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);

    let still_exists = organization::find_by_id(&db.pool, org.id).await.unwrap();
    assert!(still_exists.is_none());

    let audit_count: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) FROM audit_log WHERE action = 'organization.delete'"#
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .unwrap_or(0);
    assert_eq!(audit_count, 1, "org deletion should be audited");

    // CLAUDE.md (Organization "Not cascaded"): audit rows are "retained with
    // their org_id ... after the org is gone". The org_id must persist verbatim
    // on the forensic row — NOT be nulled (which would also conflate it with an
    // instance-wide event, where org_id IS NULL by design).
    let retained_org_id: Option<Uuid> =
        sqlx::query_scalar!(r#"SELECT org_id FROM audit_log WHERE action = 'organization.delete'"#)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        retained_org_id,
        Some(org.id),
        "org_id must be retained on the audit row after the org is hard-deleted, not nulled"
    );
}

// ---------------------------------------------------------------------------
// Owner manages members; musician is forbidden from manage routes.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn owner_can_add_a_member() {
    let db = TestDb::create_and_migrate().await;
    let owner_user = create_plain_user(&db.pool, "owner1", "correct horse battery staple").await;
    let new_member = create_plain_user(&db.pool, "newmember", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Owner Org").await;
    add_member(&db.pool, org.id, owner_user.id, membership::Role::Owner).await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, owner_user.id);

    let request = bearer_request("POST", &format!("/v1/orgs/{}/members", org.id), &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(format!(
            r#"{{"userId":"{}","role":"musician"}}"#,
            new_member.id
        )))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);

    let audit_count: i64 =
        sqlx::query_scalar!(r#"SELECT count(*) FROM audit_log WHERE action = 'membership.create'"#)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .unwrap_or(0);
    assert_eq!(audit_count, 1, "membership creation should be audited");
}

#[tokio::test]
async fn musician_gets_403_on_manage_members_route() {
    let db = TestDb::create_and_migrate().await;
    let owner_user = create_plain_user(&db.pool, "owner2", "correct horse battery staple").await;
    let musician_user =
        create_plain_user(&db.pool, "musician1", "correct horse battery staple").await;
    let target_user = create_plain_user(&db.pool, "target1", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Musician Org").await;
    add_member(&db.pool, org.id, owner_user.id, membership::Role::Owner).await;
    add_member(
        &db.pool,
        org.id,
        musician_user.id,
        membership::Role::Musician,
    )
    .await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, musician_user.id);

    let request = bearer_request("POST", &format!("/v1/orgs/{}/members", org.id), &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(format!(
            r#"{{"userId":"{}","role":"musician"}}"#,
            target_user.id
        )))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn musician_can_still_read_the_roster() {
    let db = TestDb::create_and_migrate().await;
    let owner_user = create_plain_user(&db.pool, "owner3", "correct horse battery staple").await;
    let musician_user =
        create_plain_user(&db.pool, "musician2", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Readable Org").await;
    add_member(&db.pool, org.id, owner_user.id, membership::Role::Owner).await;
    add_member(
        &db.pool,
        org.id,
        musician_user.id,
        membership::Role::Musician,
    )
    .await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, musician_user.id);

    let request = bearer_request("GET", &format!("/v1/orgs/{}/members", org.id), &token)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

#[tokio::test]
async fn outsider_cannot_read_a_roster_for_an_org_they_are_not_in() {
    let db = TestDb::create_and_migrate().await;
    let owner_user = create_plain_user(&db.pool, "owner4", "correct horse battery staple").await;
    let outsider = create_plain_user(&db.pool, "outsider1", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Private Org").await;
    add_member(&db.pool, org.id, owner_user.id, membership::Role::Owner).await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, outsider.id);

    let request = bearer_request("GET", &format!("/v1/orgs/{}/members", org.id), &token)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Last-owner invariant.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn demoting_the_last_owner_is_rejected_with_409() {
    let db = TestDb::create_and_migrate().await;
    let owner_user = create_plain_user(&db.pool, "soleowner", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Sole Owner Org").await;
    let owner_membership =
        add_member(&db.pool, org.id, owner_user.id, membership::Role::Owner).await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, owner_user.id);

    let etag = lied::listing::etag_for(owner_membership.updated_at);
    let request = bearer_request(
        "PATCH",
        &format!("/v1/orgs/{}/members/{}", org.id, owner_membership.id),
        &token,
    )
    .header(axum::http::header::CONTENT_TYPE, "application/json")
    .header(axum::http::header::IF_MATCH, etag)
    .body(axum::body::Body::from(r#"{"role":"musician"}"#))
    .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn removing_the_last_owner_is_rejected_with_409() {
    let db = TestDb::create_and_migrate().await;
    let owner_user =
        create_plain_user(&db.pool, "soleowner2", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Sole Owner Org 2").await;
    let owner_membership =
        add_member(&db.pool, org.id, owner_user.id, membership::Role::Owner).await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, owner_user.id);

    let etag = lied::listing::etag_for(owner_membership.updated_at);
    let request = bearer_request(
        "DELETE",
        &format!("/v1/orgs/{}/members/{}", org.id, owner_membership.id),
        &token,
    )
    .header(axum::http::header::IF_MATCH, etag)
    .body(axum::body::Body::empty())
    .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn demoting_one_of_two_owners_succeeds() {
    let db = TestDb::create_and_migrate().await;
    let owner_a = create_plain_user(&db.pool, "ownera", "correct horse battery staple").await;
    let owner_b = create_plain_user(&db.pool, "ownerb", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Two Owner Org").await;
    let membership_a = add_member(&db.pool, org.id, owner_a.id, membership::Role::Owner).await;
    add_member(&db.pool, org.id, owner_b.id, membership::Role::Owner).await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, owner_a.id);

    let etag = lied::listing::etag_for(membership_a.updated_at);
    let request = bearer_request(
        "PATCH",
        &format!("/v1/orgs/{}/members/{}", org.id, membership_a.id),
        &token,
    )
    .header(axum::http::header::CONTENT_TYPE, "application/json")
    .header(axum::http::header::IF_MATCH, etag)
    .body(axum::body::Body::from(r#"{"role":"musician"}"#))
    .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let role_change_count: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) FROM audit_log WHERE action = 'membership.role_change'"#
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .unwrap_or(0);
    assert_eq!(role_change_count, 1, "the role change should be audited");
}

// ---------------------------------------------------------------------------
// Pagination, sort, filter.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_orgs_paginates_and_sorts() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    create_org(&db.pool, "Alpha Orchestra").await;
    create_org(&db.pool, "Beta Band").await;
    create_org(&db.pool, "Gamma Group").await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("GET", "/v1/orgs?limit=2&offset=0&sort=name:desc", &token)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let json = body_json(response).await;
    assert_eq!(json["total"], 3);
    assert_eq!(json["limit"], 2);
    assert_eq!(json["items"][0]["name"], "Gamma Group");
    assert_eq!(json["items"][1]["name"], "Beta Band");
}

#[tokio::test]
async fn list_orgs_rejects_unknown_sort_field_with_400() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("GET", "/v1/orgs?sort=password_hash:asc", &token)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_orgs_rejects_unknown_filter_field_with_400() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("GET", "/v1/orgs?filter[is_system_admin]=true", &token)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_orgs_filters_by_name() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    create_org(&db.pool, "Vienna Philharmonic").await;
    create_org(&db.pool, "Berlin Philharmonic").await;
    create_org(&db.pool, "Local Band").await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("GET", "/v1/orgs?filter[name]=Philharmonic", &token)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["total"], 2);
}

// ---------------------------------------------------------------------------
// ETag / If-Match optimistic concurrency.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_org_returns_an_etag_header() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Etag Org").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("GET", &format!("/v1/orgs/{}", org.id), &token)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(response.headers().get(axum::http::header::ETAG).is_some());
}

#[tokio::test]
async fn patch_org_with_stale_if_match_returns_412() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Stale Org").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("PATCH", &format!("/v1/orgs/{}", org.id), &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::IF_MATCH, "\"1\"")
        .body(axum::body::Body::from(r#"{"name":"Renamed Org"}"#))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );
}

#[tokio::test]
async fn patch_org_without_if_match_returns_412() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let org = create_org(&db.pool, "No If-Match Org").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let request = bearer_request("PATCH", &format!("/v1/orgs/{}", org.id), &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(r#"{"name":"Renamed Org"}"#))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );
}

#[tokio::test]
async fn patch_org_with_correct_if_match_succeeds_and_audits() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Correct Etag Org").await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, admin.id);

    let etag = lied::listing::etag_for(org.updated_at);
    let request = bearer_request("PATCH", &format!("/v1/orgs/{}", org.id), &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::IF_MATCH, etag)
        .body(axum::body::Body::from(r#"{"name":"Renamed Org"}"#))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let json = body_json(response).await;
    assert_eq!(json["name"], "Renamed Org");

    let audit_count: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) FROM audit_log WHERE action = 'organization.update'"#
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .unwrap_or(0);
    assert_eq!(audit_count, 1, "org update should be audited");
}

// ---------------------------------------------------------------------------
// Membership validation: unknown instrument id, principal-not-subset.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_member_with_unknown_instrument_id_returns_400() {
    let db = TestDb::create_and_migrate().await;
    let owner_user = create_plain_user(&db.pool, "owner5", "correct horse battery staple").await;
    let target = create_plain_user(&db.pool, "target5", "correct horse battery staple").await;
    let org = create_org(&db.pool, "Instrument Org").await;
    add_member(&db.pool, org.id, owner_user.id, membership::Role::Owner).await;

    let (app, state) = build_test_app(db.pool.clone()).await;
    let token = mint_token(&state, owner_user.id);

    let bogus_instrument_id = Uuid::now_v7();
    let request = bearer_request("POST", &format!("/v1/orgs/{}/members", org.id), &token)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(format!(
            r#"{{"userId":"{}","role":"musician","instrumentIds":["{}"]}}"#,
            target.id, bogus_instrument_id
        )))
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Authorization helper unit-level smoke test through the real router:
// missing/garbage bearer credentials on a member-management route are 401,
// not 403/500 (the auth boundary runs before the role check).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_credentials_on_members_route_returns_401() {
    let db = TestDb::create_and_migrate().await;
    let org = create_org(&db.pool, "No Creds Org").await;
    let (app, _state) = build_test_app(db.pool.clone()).await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/v1/orgs/{}/members", org.id))
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
}
