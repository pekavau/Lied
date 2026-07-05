//! Integration tests for issue #4 (Auth): create-admin + login, the
//! mutual independence of session/bearer/app-password auth, WebDAV
//! Basic-auth via app passwords, login rate-limiting, the audit log, and a
//! real `PostgresStore` session round-trip.
//!
//! Each test spins up a fresh ephemeral Postgres via `testcontainers` (no
//! fixture pollution between tests, per CLAUDE.md Testing posture) and
//! applies every migration with `sqlx::migrate!`.

use std::net::SocketAddr;
use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::user;
use lied::state::AppState;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tower::{Service, ServiceExt};
use uuid::Uuid;

/// A fresh ephemeral Postgres container with every migration applied.
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

/// A minimal `AppConfig` for tests: no real S3 endpoint is contacted by any
/// auth flow, so the S3 fields are filled with harmless placeholders.
fn test_config() -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from("test-signing-key-material-for-auth-tests".to_string()),
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

/// Build an `AppState` directly (bypassing `AppState::connect`, which also
/// dials S3 — no auth flow under test needs MinIO reachable).
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

/// Bootstraps a system admin via `auth::admin::create_admin`, mirroring the
/// `lied-server create-admin` CLI subcommand's core logic.
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

// ---------------------------------------------------------------------------
// Item 2: create-admin bootstraps an admin, and login works against it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_admin_bootstraps_a_working_login() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;
    assert!(admin.is_system_admin);

    // A fresh, unbacked `Session` (no axum request involved) is sufficient
    // to exercise `auth::session::login` directly.
    let store = tower_sessions_sqlx_store::PostgresStore::new(db.pool.clone());
    let session = tower_sessions::Session::new(None, std::sync::Arc::new(store), None);

    let logged_in = auth::session::login(
        &db.pool,
        &session,
        "admin",
        "correct horse battery staple",
        None,
    )
    .await
    .expect("login should succeed with the bootstrap credentials");
    assert_eq!(logged_in.id, admin.id);

    let stored_user_id = auth::session::current_user_id(&session)
        .await
        .expect("session read should succeed");
    assert_eq!(stored_user_id, Some(admin.id));
}

#[tokio::test]
async fn login_rejects_wrong_password_and_audits_failure() {
    let db = TestDb::create_and_migrate().await;
    bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;

    let store = tower_sessions_sqlx_store::PostgresStore::new(db.pool.clone());
    let session = tower_sessions::Session::new(None, std::sync::Arc::new(store), None);

    let result = auth::session::login(&db.pool, &session, "admin", "wrong password", None).await;
    assert!(matches!(
        result,
        Err(auth::session::LoginError::InvalidCredentials)
    ));

    let failure_count: i64 =
        sqlx::query_scalar!(r#"SELECT count(*) FROM audit_log WHERE action = 'auth.login_failed'"#)
            .fetch_one(&db.pool)
            .await
            .expect("count audit rows")
            .unwrap_or(0);
    assert_eq!(failure_count, 1, "a failed login should be audited");
}

// ---------------------------------------------------------------------------
// Item 8: audit log entries exist with secrets redacted.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_admin_audit_row_does_not_leak_the_password_hash() {
    let db = TestDb::create_and_migrate().await;
    bootstrap_admin(&db.pool, "admin", "correct horse battery staple").await;

    let row =
        sqlx::query!(r#"SELECT action, payload FROM audit_log WHERE action = 'auth.create_admin'"#)
            .fetch_one(&db.pool)
            .await
            .expect("create_admin should write an audit row");

    let payload = row.payload.expect("payload should be present");
    let payload_str = payload.to_string();
    assert!(!payload_str.contains("correct horse battery staple"));
    assert!(
        !payload_str.to_lowercase().contains("argon2"),
        "no argon2 PHC hash material should appear in the audit payload"
    );
}

// ---------------------------------------------------------------------------
// Item 5 + acceptance criteria: app passwords are independent of sessions.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoking_an_app_password_does_not_touch_the_session() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "alice", "correct horse battery staple").await;

    // Establish a session, independent of any app password.
    let store = tower_sessions_sqlx_store::PostgresStore::new(db.pool.clone());
    let session = tower_sessions::Session::new(None, std::sync::Arc::new(store), None);
    auth::session::login(
        &db.pool,
        &session,
        "alice",
        "correct horse battery staple",
        None,
    )
    .await
    .expect("login should succeed");

    // Create and then revoke an app password.
    let generated = auth::app_password::generate().expect("generate app password");
    let app_password_id = Uuid::now_v7();
    lied::domain::app_password::create(
        &db.pool,
        app_password_id,
        admin.id,
        "test device",
        &generated.hash,
        &generated.prefix,
    )
    .await
    .expect("app password creation should succeed");

    let revoked = lied::domain::app_password::revoke(&db.pool, app_password_id, admin.id)
        .await
        .expect("revoke should succeed");
    assert!(revoked);

    // The web session is untouched by the app-password revocation.
    let still_logged_in = auth::session::current_user_id(&session)
        .await
        .expect("session read should succeed");
    assert_eq!(still_logged_in, Some(admin.id));
}

#[tokio::test]
async fn web_logout_does_not_revoke_an_app_password() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "bob", "correct horse battery staple").await;

    let generated = auth::app_password::generate().expect("generate app password");
    let app_password_id = Uuid::now_v7();
    lied::domain::app_password::create(
        &db.pool,
        app_password_id,
        admin.id,
        "bob's tablet",
        &generated.hash,
        &generated.prefix,
    )
    .await
    .expect("app password creation should succeed");

    let store = tower_sessions_sqlx_store::PostgresStore::new(db.pool.clone());
    let session = tower_sessions::Session::new(None, std::sync::Arc::new(store), None);
    auth::session::login(
        &db.pool,
        &session,
        "bob",
        "correct horse battery staple",
        None,
    )
    .await
    .expect("login should succeed");

    auth::session::logout(&db.pool, &session, None)
        .await
        .expect("logout should succeed");

    // The app password is still active (not revoked) after a web logout.
    let still_active: Option<time::OffsetDateTime> = sqlx::query_scalar!(
        r#"SELECT revoked_at FROM app_password WHERE id = $1"#,
        app_password_id
    )
    .fetch_one(&db.pool)
    .await
    .expect("fetch app password");
    assert!(
        still_active.is_none(),
        "logout must not revoke any app password"
    );
}

// ---------------------------------------------------------------------------
// WebDAV: PROPFIND-equivalent auth gate, via the real axum router.
// ---------------------------------------------------------------------------

async fn build_test_app(pool: PgPool) -> axum::Router {
    let config = test_config();
    let state = test_state(pool, config);
    lied::routes::build_router(state)
}

/// Attach a `ConnectInfo<SocketAddr>` to a request so a route carrying the
/// login rate-limiter can extract a client IP under `oneshot`. The limiter's
/// `SmartIpKeyExtractor` keys on the forwarded headers or the peer IP; in
/// production that peer IP arrives via `into_make_service_with_connect_info`,
/// which `oneshot` does not wire up, so without this the limited routes
/// (`POST /admin/login`, `POST /v1/tokens`) 500 instead of reaching the
/// handler.
fn with_peer_ip<B>(mut request: axum::http::Request<B>) -> axum::http::Request<B> {
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            9000,
        ))));
    request
}

#[tokio::test]
async fn webdav_request_without_app_password_is_rejected_with_401() {
    let db = TestDb::create_and_migrate().await;
    let app = build_test_app(db.pool.clone()).await;

    let request = axum::http::Request::builder()
        .method("PROPFIND")
        .uri("/orgs")
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert!(
        response
            .headers()
            .get(axum::http::header::WWW_AUTHENTICATE)
            .is_some(),
        "a 401 WebDAV response must carry WWW-Authenticate: Basic"
    );
}

#[tokio::test]
async fn webdav_request_with_valid_app_password_reaches_the_handler() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "carol", "correct horse battery staple").await;

    let generated = auth::app_password::generate().expect("generate app password");
    lied::domain::app_password::create(
        &db.pool,
        Uuid::now_v7(),
        admin.id,
        "carol's ipad",
        &generated.hash,
        &generated.prefix,
    )
    .await
    .expect("app password creation should succeed");

    let app = build_test_app(db.pool.clone()).await;

    let credentials = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("carol:{}", generated.plaintext),
    );
    let request = axum::http::Request::builder()
        .method("PROPFIND")
        .uri("/orgs")
        // Real WebDAV clients send a finite Depth; dav-server correctly refuses
        // infinite-depth PROPFIND on a collection with 403, so a missing Depth
        // header is not a meaningful "reaches the handler" signal.
        .header("Depth", "1")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Basic {credentials}"),
        )
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_ne!(
        response.status(),
        axum::http::StatusCode::UNAUTHORIZED,
        "a valid app password must not be rejected"
    );
    // 207 Multi-Status: the request authenticated and reached the real DavFs
    // (the `/orgs` collection lists, empty here since this user has no orgs).
    assert_eq!(response.status().as_u16(), 207);
}

#[tokio::test]
async fn webdav_app_password_use_is_audited_and_touches_last_used_at() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "dana", "correct horse battery staple").await;

    let generated = auth::app_password::generate().expect("generate app password");
    let app_password_id = Uuid::now_v7();
    lied::domain::app_password::create(
        &db.pool,
        app_password_id,
        admin.id,
        "dana's phone",
        &generated.hash,
        &generated.prefix,
    )
    .await
    .expect("app password creation should succeed");

    let result = auth::webdav::authenticate(
        &db.pool,
        Some(&format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("dana:{}", generated.plaintext)
            )
        )),
        None,
    )
    .await
    .expect("authentication should succeed");
    assert_eq!(result.user_id, admin.id);

    let last_used_at: Option<time::OffsetDateTime> = sqlx::query_scalar!(
        r#"SELECT last_used_at FROM app_password WHERE id = $1"#,
        app_password_id
    )
    .fetch_one(&db.pool)
    .await
    .expect("fetch app password");
    assert!(last_used_at.is_some());

    let audited: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) FROM audit_log WHERE action = 'auth.app_password_used'"#
    )
    .fetch_one(&db.pool)
    .await
    .expect("count audit rows")
    .unwrap_or(0);
    assert_eq!(audited, 1);
}

// ---------------------------------------------------------------------------
// Bearer tokens authenticate `/v1` requests; expired/invalid tokens 401.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bearer_token_authenticates_v1_instruments() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "erin", "correct horse battery staple").await;

    let config = test_config();
    let keyring = auth::jwt::Keyring::from_single_key(
        config.jwt_signing_key.expose(),
        config.jwt_lifetime_days,
    );
    let (token, _claims) = keyring.mint(admin.id, None).expect("mint token");

    let app = build_test_app(db.pool.clone()).await;
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/instruments")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

/// Regression: `POST /v1/tokens` must work through the *real* `/v1` router,
/// not just via a directly-constructed `Keyring`. The handler uses a bare
/// `tower_sessions::Session` extractor, which 500s unless the `/v1` tree
/// carries a `SessionManagerLayer` — a wiring gap the keyring-only tests
/// above could not catch (it only surfaced minting over HTTP). This test
/// exercises the username+password mint path end-to-end and then spends the
/// returned token on a gated endpoint.
#[tokio::test]
async fn post_v1_tokens_mints_a_usable_bearer_token() {
    let db = TestDb::create_and_migrate().await;
    bootstrap_admin(&db.pool, "grace", "correct horse battery staple").await;

    let app = build_test_app(db.pool.clone()).await;

    // 1. Mint a token with username + password (the pure-programmatic path).
    let mint_request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/tokens")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            r#"{"username":"grace","password":"correct horse battery staple"}"#,
        ))
        .unwrap();

    let mint_response = app
        .clone()
        .oneshot(with_peer_ip(mint_request))
        .await
        .expect("mint request should run");
    assert_eq!(
        mint_response.status(),
        axum::http::StatusCode::OK,
        "POST /v1/tokens must succeed (not 500 from a missing session layer)"
    );

    let body = axum::body::to_bytes(mint_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let token = json["token"].as_str().expect("response carries a token");

    // 2. Spend it on a gated endpoint to prove it's a real, valid token.
    let use_request = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/instruments")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let use_response = app
        .oneshot(use_request)
        .await
        .expect("instruments request should run");
    assert_eq!(use_response.status(), axum::http::StatusCode::OK);
}

/// Wrong credentials to `POST /v1/tokens` must be rejected with 401, not a
/// 500 — the same router-wiring regression surface as the success path.
#[tokio::test]
async fn post_v1_tokens_with_bad_password_returns_401() {
    let db = TestDb::create_and_migrate().await;
    bootstrap_admin(&db.pool, "heidi", "correct horse battery staple").await;

    let app = build_test_app(db.pool.clone()).await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/tokens")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            r#"{"username":"heidi","password":"wrong"}"#,
        ))
        .unwrap();

    let response = app
        .oneshot(with_peer_ip(request))
        .await
        .expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
}

/// Creating two app passwords with the same name for one user clashes on the
/// `(user_id, name)` unique index. The contract (API guidelines §4.1, and the
/// `#[utoipa::path]` on the handler) is `409 Conflict` with an RFC 7807 body —
/// not `412`, which is reserved for `If-Match` preconditions this endpoint
/// doesn't have. Regression guard for the spec-vs-behavior drift fixed here.
#[tokio::test]
async fn creating_a_duplicate_app_password_name_returns_409() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "ivan", "correct horse battery staple").await;

    let config = test_config();
    let keyring = auth::jwt::Keyring::from_single_key(
        config.jwt_signing_key.expose(),
        config.jwt_lifetime_days,
    );
    let (token, _claims) = keyring.mint(admin.id, None).expect("mint token");

    let app = build_test_app(db.pool.clone()).await;

    let make_request = || {
        axum::http::Request::builder()
            .method("POST")
            .uri("/v1/app-passwords")
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                r#"{"name":"iPad in rehearsal room"}"#,
            ))
            .unwrap()
    };

    // First creation succeeds.
    let first = app
        .clone()
        .oneshot(make_request())
        .await
        .expect("first request should run");
    assert_eq!(first.status(), axum::http::StatusCode::CREATED);

    // Second creation with the same name clashes → 409 Conflict, problem+json.
    let second = app
        .oneshot(make_request())
        .await
        .expect("second request should run");
    assert_eq!(
        second.status(),
        axum::http::StatusCode::CONFLICT,
        "a duplicate app-password name must be 409, not 412"
    );
    assert_eq!(
        second
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "error body must be RFC 7807 Problem Details"
    );
}

#[tokio::test]
async fn missing_credentials_on_v1_instruments_returns_401() {
    let db = TestDb::create_and_migrate().await;
    let app = build_test_app(db.pool.clone()).await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/instruments")
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn garbage_bearer_token_on_v1_instruments_returns_401() {
    let db = TestDb::create_and_migrate().await;
    let app = build_test_app(db.pool.clone()).await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/instruments")
        .header(axum::http::header::AUTHORIZATION, "Bearer not-a-real-jwt")
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_bearer_token_on_v1_instruments_returns_401() {
    let db = TestDb::create_and_migrate().await;
    let admin = bootstrap_admin(&db.pool, "frank", "correct horse battery staple").await;

    // A keyring with a negative lifetime mints already-expired tokens.
    let config = test_config();
    let expired_keyring = auth::jwt::Keyring::from_single_key(config.jwt_signing_key.expose(), -1);
    let (token, _claims) = expired_keyring.mint(admin.id, None).expect("mint token");

    let app = build_test_app(db.pool.clone()).await;
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/instruments")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Login rate limiting: repeated failed logins are throttled.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_login_attempts_are_eventually_rate_limited() {
    let db = TestDb::create_and_migrate().await;
    bootstrap_admin(&db.pool, "grace", "correct horse battery staple").await;

    let mut config = test_config();
    config.ratelimit_login_per_min = 3;
    let state = test_state(db.pool.clone(), config);
    let app = lied::routes::build_router(state).into_make_service_with_connect_info::<SocketAddr>();

    let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
    let mut saw_429 = false;

    for _ in 0..10 {
        let mut svc = app.clone().call(peer).await.expect("make service");
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/admin/login")
            .header(
                axum::http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(axum::body::Body::from("username=grace&password=wrong"))
            .unwrap();

        let response = svc.call(request).await.expect("request should run");
        if response.status() == axum::http::StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            break;
        }
    }

    assert!(
        saw_429,
        "repeated login attempts from the same IP should eventually be rate-limited"
    );
}

/// Regression (#1): `POST /v1/tokens` runs the same password verify as
/// `/admin/login`, so it must share the per-IP login rate limit. Before the
/// fix this endpoint had no limiter — every attempt returned 401, never 429 —
/// making it an unthrottled brute-force bypass.
#[tokio::test]
async fn repeated_token_mint_attempts_are_rate_limited() {
    let db = TestDb::create_and_migrate().await;
    bootstrap_admin(&db.pool, "judy", "correct horse battery staple").await;

    let mut config = test_config();
    config.ratelimit_login_per_min = 3;
    let state = test_state(db.pool.clone(), config);
    let app = lied::routes::build_router(state).into_make_service_with_connect_info::<SocketAddr>();

    let peer: SocketAddr = "127.0.0.1:12346".parse().unwrap();
    let mut saw_429 = false;

    for _ in 0..10 {
        let mut svc = app.clone().call(peer).await.expect("make service");
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/tokens")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                r#"{"username":"judy","password":"wrong"}"#,
            ))
            .unwrap();

        let response = svc.call(request).await.expect("request should run");
        if response.status() == axum::http::StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            break;
        }
    }

    assert!(
        saw_429,
        "POST /v1/tokens must be rate-limited like /admin/login (brute-force surface)"
    );
}

/// Read the `lied_session` cookie value out of a response's `Set-Cookie`
/// headers, if present.
fn session_cookie_value(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|cookie| {
            cookie
                .strip_prefix("lied_session=")
                .map(|rest| rest.split(';').next().unwrap_or("").to_string())
        })
}

/// Regression (#3): a successful login must rotate the session id
/// (`session.cycle_id()`), so a session id an attacker planted in the
/// victim's browser before login cannot be reused afterward (session
/// fixation). We establish an anonymous session (the login page stores a
/// CSRF token), then log in carrying that cookie and assert the post-login
/// session id differs.
#[tokio::test]
async fn login_rotates_the_session_id_to_prevent_fixation() {
    let db = TestDb::create_and_migrate().await;
    bootstrap_admin(&db.pool, "ivan", "correct horse battery staple").await;
    let app = build_test_app(db.pool.clone()).await;

    // 1. Load the login page to get an anonymous session cookie.
    let get_request = axum::http::Request::builder()
        .method("GET")
        .uri("/admin/login")
        .body(axum::body::Body::empty())
        .unwrap();
    let get_response = app
        .clone()
        .oneshot(get_request)
        .await
        .expect("login page request should run");
    let pre_login = session_cookie_value(&get_response)
        .expect("the login page should establish a session cookie");

    // 2. Log in carrying that cookie. A fixation-safe login issues a *new*
    //    session id on the response.
    let post_request = axum::http::Request::builder()
        .method("POST")
        .uri("/admin/login")
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(
            axum::http::header::COOKIE,
            format!("lied_session={pre_login}"),
        )
        .body(axum::body::Body::from(
            "username=ivan&password=correct horse battery staple",
        ))
        .unwrap();
    let post_response = app
        .oneshot(with_peer_ip(post_request))
        .await
        .expect("login request should run");
    assert!(
        post_response.status().is_redirection(),
        "a successful login should redirect, got {}",
        post_response.status()
    );
    let post_login = session_cookie_value(&post_response)
        .expect("a successful login should set a session cookie");

    assert_ne!(
        pre_login, post_login,
        "login must rotate the session id (session-fixation defense)"
    );
}

// ---------------------------------------------------------------------------
// Item 10: real PostgresStore session round-trip (save -> load -> delete).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn postgres_session_store_round_trips_through_the_session_store_trait() {
    use tower_sessions::session::{Id, Record};
    use tower_sessions::session_store::SessionStore;
    use tower_sessions_sqlx_store::PostgresStore;

    let db = TestDb::create_and_migrate().await;
    let store = PostgresStore::new(db.pool.clone());

    let mut record = Record {
        id: Id::default(),
        data: std::collections::HashMap::from([(
            "user_id".to_string(),
            serde_json::json!(Uuid::now_v7()),
        )]),
        expiry_date: time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    };

    store
        .create(&mut record)
        .await
        .expect("create should succeed against the real PostgresStore");

    let loaded = store
        .load(&record.id)
        .await
        .expect("load should succeed")
        .expect("the record should exist after create");
    assert_eq!(loaded.id, record.id);
    assert_eq!(loaded.data, record.data);

    store
        .delete(&record.id)
        .await
        .expect("delete should succeed");

    let after_delete = store
        .load(&record.id)
        .await
        .expect("load after delete should not error");
    assert!(
        after_delete.is_none(),
        "the record must be gone after delete"
    );
}

// ---------------------------------------------------------------------------
// Review fixes: audit request_id correlates with the request; CSRF cookie set.
// ---------------------------------------------------------------------------

/// Regression: an audit row's `request_id` must equal the `x-request-id` the
/// request was assigned (the value the tracing span logs), so a row can be
/// joined back to its log line. Before the fix the handlers minted a fresh,
/// unrelated UUID at the audit call site, so the column never matched.
#[tokio::test]
async fn audit_request_id_matches_the_response_request_id() {
    let db = TestDb::create_and_migrate().await;
    bootstrap_admin(&db.pool, "peggy", "correct horse battery staple").await;
    let app = build_test_app(db.pool.clone()).await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/admin/login")
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(axum::body::Body::from(
            "username=peggy&password=correct horse battery staple",
        ))
        .unwrap();

    let response = app
        .oneshot(with_peer_ip(request))
        .await
        .expect("login request should run");
    assert!(response.status().is_redirection());

    let header_request_id = response
        .headers()
        .get(lied::routes::REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("the response should carry a parseable x-request-id");

    let audited_request_id: Option<Uuid> = sqlx::query_scalar!(
        r#"SELECT request_id FROM audit_log WHERE action = 'auth.login_succeeded'"#
    )
    .fetch_one(&db.pool)
    .await
    .expect("a successful login should write an audit row");

    assert_eq!(
        audited_request_id,
        Some(header_request_id),
        "the audit row's request_id must match the request's x-request-id"
    );
}

/// Regression: a safe-method request through the `/admin` CSRF middleware must
/// receive the readable `lied_csrf` cookie (the second half of the
/// double-submit pair). Before the fix the token was only stored in the
/// session and never handed to the client, so no HTMX request could ever echo
/// it back in `HX-CSRF`.
#[tokio::test]
async fn safe_request_sets_the_readable_csrf_cookie() {
    let db = TestDb::create_and_migrate().await;
    let app = build_test_app(db.pool.clone()).await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/admin/login")
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.expect("request should run");

    let sets_csrf_cookie = response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|cookie| cookie.starts_with("lied_csrf="));

    assert!(
        sets_csrf_cookie,
        "a GET through the admin CSRF middleware must set the readable lied_csrf cookie"
    );
}
