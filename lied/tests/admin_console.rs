//! Integration tests for issue #30 (Phase 2 console foundation): the
//! `ConsoleCtx` permission-matrix gating and the principal carve-out, against
//! a real Postgres. These drive `ConsoleCtx::load` directly (the testing
//! posture explicitly allows exercising internals, not only black-box HTTP) —
//! the `/admin` tree is session-cookie authed and has no bearer-token path, so
//! there is no lightweight way to log in over HTTP. One HTTP-level test
//! confirms the routes are wired behind the auth gate.
//!
//! Fixture/harness pattern duplicated from `tests/orgs.rs` (the established
//! convention in this repo): a fresh ephemeral Postgres per test.

use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{membership, organization, user};
use lied::error::AppError;
use lied::routes::admin::console::ConsoleCtx;
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
        jwt_signing_key: Secret::from("test-signing-key-material-for-console-tests".to_string()),
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

fn test_state(pool: PgPool) -> AppState {
    let config = test_config();
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

async fn create_user(pool: &PgPool, username: &str, is_system_admin: bool) -> user::User {
    let hash = auth::password::hash_password("password").expect("hash password");
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
        is_system_admin,
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
    is_principal: bool,
) {
    membership::create(
        pool,
        Uuid::now_v7(),
        user_id,
        org_id,
        membership::MembershipFields {
            role,
            instrument_ids: &[],
            is_principal,
            principal_instrument_ids: &[],
        },
        None,
    )
    .await
    .expect("create membership");
}

#[tokio::test]
async fn staff_roles_enter_with_matrix_capabilities() {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let org = create_org(&db.pool, "Acme Orchestra").await;

    // Owner: everything.
    let owner = create_user(&db.pool, "owner", false).await;
    add_member(&db.pool, org.id, owner.id, membership::Role::Owner, false).await;
    let ctx = ConsoleCtx::load(&state, &owner, org.id)
        .await
        .expect("owner enters");
    assert!(ctx.is_staff());
    assert!(ctx.can_edit_arrangements());
    assert!(ctx.can_manage_members());
    assert!(ctx.can_author_global_annotations());
    assert!(ctx.can_view_coverage());

    // Archivist: edits content, but not members or annotations.
    let archivist = create_user(&db.pool, "archivist", false).await;
    add_member(
        &db.pool,
        org.id,
        archivist.id,
        membership::Role::Archivist,
        false,
    )
    .await;
    let ctx = ConsoleCtx::load(&state, &archivist, org.id)
        .await
        .expect("archivist enters");
    assert!(ctx.is_staff());
    assert!(ctx.can_edit_arrangements());
    assert!(!ctx.can_manage_members());
    assert!(!ctx.can_author_global_annotations());

    // Conductor: plans and annotates, but cannot edit arrangements or members.
    let conductor = create_user(&db.pool, "conductor", false).await;
    add_member(
        &db.pool,
        org.id,
        conductor.id,
        membership::Role::Conductor,
        false,
    )
    .await;
    let ctx = ConsoleCtx::load(&state, &conductor, org.id)
        .await
        .expect("conductor enters");
    assert!(ctx.is_staff());
    assert!(!ctx.can_edit_arrangements());
    assert!(ctx.can_build_collections());
    assert!(ctx.can_author_global_annotations());
    assert!(!ctx.can_manage_members());
}

#[tokio::test]
async fn plain_musician_is_denied_console_access() {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let org = create_org(&db.pool, "Acme Orchestra").await;
    let musician = create_user(&db.pool, "musician", false).await;
    add_member(
        &db.pool,
        org.id,
        musician.id,
        membership::Role::Musician,
        false,
    )
    .await;

    let result = ConsoleCtx::load(&state, &musician, org.id).await;
    assert!(
        matches!(result, Err(AppError::Forbidden)),
        "plain musician must be denied console access"
    );
}

#[tokio::test]
async fn principal_musician_enters_but_only_sees_coverage() {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let org = create_org(&db.pool, "Acme Orchestra").await;
    let principal = create_user(&db.pool, "principal", false).await;
    add_member(
        &db.pool,
        org.id,
        principal.id,
        membership::Role::Musician,
        true,
    )
    .await;

    let ctx = ConsoleCtx::load(&state, &principal, org.id)
        .await
        .expect("principal enters");
    // The carve-out: coverage yes, everything else no.
    assert!(ctx.can_view_coverage());
    assert!(!ctx.is_staff());
    assert!(!ctx.can_edit_arrangements());
    assert!(!ctx.can_build_collections());
    assert!(!ctx.can_author_global_annotations());
    assert!(!ctx.can_manage_members());
}

#[tokio::test]
async fn non_member_is_denied() {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let org = create_org(&db.pool, "Acme Orchestra").await;
    let stranger = create_user(&db.pool, "stranger", false).await;

    let result = ConsoleCtx::load(&state, &stranger, org.id).await;
    assert!(
        matches!(result, Err(AppError::Forbidden)),
        "non-member must be denied"
    );
}

#[tokio::test]
async fn system_admin_enters_any_org_as_owner_equivalent() {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let org = create_org(&db.pool, "Acme Orchestra").await;
    // A system admin with NO membership in the org.
    let admin = create_user(&db.pool, "sysadmin", true).await;

    let ctx = ConsoleCtx::load(&state, &admin, org.id)
        .await
        .expect("system admin enters");
    assert!(ctx.is_staff());
    assert!(ctx.can_edit_arrangements());
    assert!(ctx.can_manage_members());
    assert!(ctx.can_author_global_annotations());
    assert!(ctx.can_view_coverage());
}

#[tokio::test]
async fn missing_org_is_not_found() {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let admin = create_user(&db.pool, "sysadmin", true).await;

    let result = ConsoleCtx::load(&state, &admin, Uuid::now_v7()).await;
    assert!(
        matches!(result, Err(AppError::NotFound)),
        "missing org must be 404"
    );
}

#[tokio::test]
async fn unauthenticated_console_access_redirects_to_login() {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let org = create_org(&db.pool, "Acme Orchestra").await;
    let app = lied::routes::build_router(state);

    // Unauthenticated GET to a console section: rather than emit a raw
    // Problem Details 401 into the browser, the console redirects to the login
    // page (the `/admin` tree's HTML-response convention). This also confirms
    // the route is mounted behind the admin middleware stack.
    let request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/admin/orgs/{}/coverage", org.id))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app
        .oneshot(request)
        .await
        .expect("router handles the request");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SEE_OTHER,
        "unauthenticated console access must redirect, not 401"
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/admin/login"),
        "redirect must target the login page"
    );
}
