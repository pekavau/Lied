//! Integration tests for issue #119: the owner's org settings screen
//! (`/admin/orgs/{org}/settings`), where the owner opens the archive to
//! musicians. Drives the console over HTTP with a session cookie and the
//! CSRF field the page renders. Postgres only.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use base64::Engine;
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

const PASSWORD: &str = "console-test-password";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Ctx {
    _pg: ContainerAsync<Postgres>,
    pool: PgPool,
    app: axum::Router,
    state: AppState,
}

impl Ctx {
    async fn new() -> Self {
        Self::with_page_size(200).await
    }

    async fn with_page_size(max_page_size: u32) -> Self {
        let pg = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("start postgres");
        let port = pg.get_host_port_ipv4(5432).await.expect("pg port");
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&format!(
                "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
            ))
            .await
            .expect("connect pg");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");

        let state = test_state(pool.clone(), test_config(max_page_size));
        let app = lied::routes::build_router(state.clone());
        Self {
            _pg: pg,
            pool,
            app,
            state,
        }
    }
}

fn test_config(max_page_size: u32) -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        s3_version_retention_days: 30,
        jwt_signing_key: Secret::from("test-signing-key-material-collections".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes: 200 * 1024 * 1024,
        max_request_bytes: 256 * 1024,
        max_files_per_voice: 50,
        max_arrangements_per_org: None,
        max_page_size,
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

async fn seed_user(state: &AppState, username: &str) -> Uuid {
    let hash = auth::password::hash_password(PASSWORD).expect("hash");
    let id = Uuid::now_v7();
    user::create(
        &state.db,
        id,
        &user::slugify(username),
        username,
        None,
        Some(&hash),
        username,
        false,
        None,
    )
    .await
    .expect("user");
    id
}

fn with_peer_ip<B>(mut request: axum::http::Request<B>) -> axum::http::Request<B> {
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            9000,
        ))));
    request
}

struct Browser {
    cookie: String,
}

impl Browser {
    async fn login(app: &axum::Router, username: &str) -> Self {
        let request = with_peer_ip(
            axum::http::Request::builder()
                .method("POST")
                .uri("/admin/login")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(axum::body::Body::from(format!(
                    "username={username}&password={PASSWORD}"
                )))
                .unwrap(),
        );
        let response = app.clone().oneshot(request).await.expect("login runs");
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SEE_OTHER,
            "login must redirect on success"
        );
        let cookie = response
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .find(|c| c.starts_with(auth::session::SESSION_COOKIE_NAME))
            .expect("login must set a session cookie")
            .to_string();
        Self { cookie }
    }

    fn get(&self, uri: &str) -> axum::http::Request<axum::body::Body> {
        with_peer_ip(
            axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .header(axum::http::header::COOKIE, &self.cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
    }

    fn post(&self, uri: &str, body: String) -> axum::http::Request<axum::body::Body> {
        with_peer_ip(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header(axum::http::header::COOKIE, &self.cookie)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
    }
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn load_page(
    app: &axum::Router,
    browser: &Browser,
    uri: &str,
) -> (axum::http::StatusCode, String) {
    let response = app
        .clone()
        .oneshot(browser.get(uri))
        .await
        .expect("page request runs");
    (response.status(), body_text(response).await)
}

/// The CSRF token a page embeds in its hidden field.
fn form_csrf(html: &str) -> String {
    let marker = "name=\"csrf_token\" value=\"";
    let start = html.find(marker).expect("page must render a csrf field") + marker.len();
    html[start..]
        .split('"')
        .next()
        .expect("token is quoted")
        .to_string()
}

/// The session's CSRF token from the readable cookie the middleware sets on
/// safe requests — for callers whose page renders no form.
async fn session_csrf(app: &axum::Router, browser: &Browser, uri: &str) -> String {
    let response = app
        .clone()
        .oneshot(browser.get(uri))
        .await
        .expect("page request runs");
    response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find_map(|c| c.strip_prefix("lied_csrf="))
        .expect("safe requests must hand the client a readable CSRF cookie")
        .to_string()
}

async fn post_form(
    app: &axum::Router,
    browser: &Browser,
    uri: &str,
    fields: &[(&str, &str)],
    token: &str,
) -> axum::response::Response {
    let mut body = format!("csrf_token={token}");
    for (k, v) in fields {
        body.push('&');
        body.push_str(&form_urlencoded(k, v));
    }
    app.clone()
        .oneshot(browser.post(uri, body))
        .await
        .expect("form post runs")
}

fn form_urlencoded(key: &str, value: &str) -> String {
    let encode = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".to_string(),
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };
    format!("{}={}", encode(key), encode(value))
}

async fn audit_count(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar(r#"SELECT count(*) FROM audit_log WHERE action = $1"#)
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The plaintext token a create response shows once.
fn shown_token(html: &str) -> String {
    let start = html.find("lied_").expect("the new token is shown");
    html[start..]
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .next()
        .unwrap()
        .to_string()
}

async fn webdav_login(pool: &PgPool, username: &str, token: &str) -> bool {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"));
    auth::webdav::authenticate(pool, Some(&format!("Basic {basic}")), None)
        .await
        .is_ok()
}

/// A user whose only membership is plain `musician` — refused by every org
/// workspace, which is exactly who needs this page.
async fn musician(ctx: &Ctx, username: &str) -> Uuid {
    let uid = seed_user(&ctx.state, username).await;
    let org = organization::create(
        &ctx.pool,
        Uuid::now_v7(),
        "Town Band",
        &organization::slugify("Town Band"),
        None,
    )
    .await
    .expect("org");
    membership::create(
        &ctx.pool,
        Uuid::now_v7(),
        uid,
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
    .expect("membership");
    uid
}

/// A user with `role` in `org_id`.
async fn member(ctx: &Ctx, org_id: Uuid, username: &str, role: membership::Role) -> Uuid {
    let uid = seed_user(&ctx.state, username).await;
    membership::create(
        &ctx.pool,
        Uuid::now_v7(),
        uid,
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
    .expect("membership");
    uid
}

async fn archive_open(ctx: &Ctx, org_id: Uuid) -> bool {
    organization::find_by_id(&ctx.pool, org_id)
        .await
        .unwrap()
        .unwrap()
        .musicians_read_archive
}

/// The value of the hidden `expected_version` field the page renders.
fn expected_version(html: &str) -> String {
    let marker = "name=\"expected_version\" value=\"";
    let start = html.find(marker).expect("version field") + marker.len();
    html[start..].split('"').next().unwrap().to_string()
}

/// The owner opens and closes the archive from *Settings*, each change
/// audited; a stale form is refused; nobody else gets the screen.
#[tokio::test]
async fn the_owner_opens_the_archive_from_settings() {
    let ctx = Ctx::new().await;
    let org = organization::create(
        &ctx.pool,
        Uuid::now_v7(),
        "Town Band",
        &organization::slugify("Town Band"),
        None,
    )
    .await
    .expect("org");
    member(&ctx, org.id, "olga", membership::Role::Owner).await;
    member(&ctx, org.id, "arch", membership::Role::Archivist).await;
    member(&ctx, org.id, "carl", membership::Role::Conductor).await;
    let uri = format!("/admin/orgs/{}/settings", org.id);

    let owner = Browser::login(&ctx.app, "olga").await;
    let (status, html) =
        load_page(&ctx.app, &owner, &format!("/admin/orgs/{}/console", org.id)).await;
    assert_eq!(status, 200);
    assert!(
        html.contains(&uri),
        "the owner's nav links Settings:\n{html}"
    );

    let (status, html) = load_page(&ctx.app, &owner, &uri).await;
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("Musicians may read the whole archive"),
        "{html}"
    );
    assert!(!html.contains("checked"), "off by default:\n{html}");
    let token = form_csrf(&html);
    let version = expected_version(&html);

    // Tick the box.
    let response = post_form(
        &ctx.app,
        &owner,
        &uri,
        &[
            ("expected_version", &version),
            ("musicians_read_archive", "on"),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(archive_open(&ctx, org.id).await);
    assert_eq!(
        audit_count(&ctx.pool, "organization.settings_change").await,
        1
    );
    let (_, html) = load_page(&ctx.app, &owner, &uri).await;
    assert!(html.contains("checked"), "{html}");

    // The old form is stale now.
    let response = post_form(
        &ctx.app,
        &owner,
        &uri,
        &[("expected_version", &version)],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );
    assert!(archive_open(&ctx, org.id).await);

    // Untick it with a fresh form.
    let version = expected_version(&html);
    let response = post_form(
        &ctx.app,
        &owner,
        &uri,
        &[("expected_version", &version)],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(!archive_open(&ctx, org.id).await);
    assert_eq!(
        audit_count(&ctx.pool, "organization.settings_change").await,
        2
    );

    // Other staff: no link, no screen, no change.
    for username in ["arch", "carl"] {
        let browser = Browser::login(&ctx.app, username).await;
        let (_, home) = load_page(
            &ctx.app,
            &browser,
            &format!("/admin/orgs/{}/console", org.id),
        )
        .await;
        assert!(
            !home.contains(&uri),
            "{username} must not see Settings:\n{home}"
        );
        let (status, _) = load_page(&ctx.app, &browser, &uri).await;
        assert_eq!(status, 403, "{username}");
        let token = session_csrf(&ctx.app, &browser, "/admin").await;
        let org_now = organization::find_by_id(&ctx.pool, org.id)
            .await
            .unwrap()
            .unwrap();
        let version = org_now.updated_at.timestamp_millis().to_string();
        let response = post_form(
            &ctx.app,
            &browser,
            &uri,
            &[
                ("expected_version", &version),
                ("musicians_read_archive", "on"),
            ],
            &token,
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::FORBIDDEN,
            "{username}"
        );
        assert!(!archive_open(&ctx, org.id).await, "{username}");
    }
    assert_eq!(
        audit_count(&ctx.pool, "organization.settings_change").await,
        2
    );
}
