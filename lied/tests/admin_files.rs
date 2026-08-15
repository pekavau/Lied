//! Integration tests for issue #32 (console file-management screens).
//!
//! These drive the `/admin` tree over real HTTP (a session-cookie login, then
//! the rendered form contracts — the CSRF hidden field and the `?csrf=` query
//! token the multipart forms carry), against a real Postgres **and** a real
//! MinIO. The `/v1` file suite (`tests/files.rs`) already covers the shared
//! `file_service`; what is exercised here is the console-specific half that
//! only the browser surface has:
//!   - role gating on the screens (archivist writes, conductor read-only,
//!     plain musician denied);
//!   - the CSRF contract for streaming multipart posts;
//!   - upload → replace → delete → restore round trip incl. audit;
//!   - the friendly 415 / 413 error pages;
//!   - filenames can't inject path segments into the derived storage key.

use std::net::SocketAddr;
use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{arrangement, file, membership, organization, user, voice};
use lied::state::AppState;
use lied::storage;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

const BUCKET: &str = "lied-test";
const BOUNDARY: &str = "X-LIED-CONSOLE-BOUNDARY";
const PASSWORD: &str = "console-test-password";
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n";
const PDF_V2: &[u8] = b"%PDF-1.4\nreplacement bytes\ntrailer<<>>\n%%EOF\n";

// ---------------------------------------------------------------------------
// Harness (Postgres + MinIO, as in tests/files.rs)
// ---------------------------------------------------------------------------

struct Ctx {
    _pg: ContainerAsync<Postgres>,
    _minio: ContainerAsync<MinIO>,
    pool: PgPool,
    app: axum::Router,
    state: AppState,
}

impl Ctx {
    async fn new(max_upload_bytes: u64) -> Self {
        let pg = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("start postgres");
        let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&format!(
                "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
            ))
            .await
            .expect("connect pg");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");

        let minio = MinIO::default().start().await.expect("start minio");
        let minio_port = minio.get_host_port_ipv4(9000).await.expect("minio port");
        let endpoint = format!("http://127.0.0.1:{minio_port}");

        let state = test_state(pool.clone(), test_config(endpoint, max_upload_bytes));
        state
            .s3
            .create_bucket()
            .bucket(BUCKET)
            .send()
            .await
            .expect("create bucket");

        let app = lied::routes::build_router(state.clone());
        Self {
            _pg: pg,
            _minio: minio,
            pool,
            app,
            state,
        }
    }
}

fn test_config(s3_endpoint: String, max_upload_bytes: u64) -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint,
        s3_bucket: BUCKET.to_string(),
        s3_access_key_id: Secret::from("minioadmin".to_string()),
        s3_secret_access_key: Secret::from("minioadmin".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from("test-signing-key-material-console-files".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes,
        max_request_bytes: 256 * 1024,
        max_files_per_voice: 50,
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An org plus one member per role we care about, all sharing [`PASSWORD`].
struct Fixture {
    org_id: Uuid,
    arr_id: Uuid,
    voice_id: Uuid,
    org_slug: String,
    arr_slug: String,
    voice_slug: String,
}

async fn seed(state: &AppState) -> Fixture {
    let org_id = Uuid::now_v7();
    let org_slug = organization::slugify("Console Orchestra");
    organization::create(&state.db, org_id, "Console Orchestra", &org_slug, None)
        .await
        .expect("org");

    for (username, role) in [
        ("arch", membership::Role::Archivist),
        ("cond", membership::Role::Conductor),
        ("mus", membership::Role::Musician),
    ] {
        let uid = seed_user(state, username).await;
        membership::create(
            &state.db,
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
    }

    let arr_id = Uuid::now_v7();
    let arr_slug = arrangement::slugify("Overture");
    arrangement::create(
        &state.db,
        arr_id,
        org_id,
        &arr_slug,
        arrangement::ArrangementFields {
            title: "Overture",
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
        },
        None,
    )
    .await
    .expect("arrangement");

    let voice_id = Uuid::now_v7();
    let voice_slug = voice::slugify("Flute 1");
    let instrument_id = sqlx::query_scalar!(r#"SELECT id FROM instrument ORDER BY key LIMIT 1"#)
        .fetch_one(&state.db)
        .await
        .expect("instrument");
    voice::create(
        &state.db,
        voice_id,
        arr_id,
        "Flute 1",
        &voice_slug,
        instrument_id,
        None,
    )
    .await
    .expect("voice");

    Fixture {
        org_id,
        arr_id,
        voice_id,
        org_slug,
        arr_slug,
        voice_slug,
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

// ---------------------------------------------------------------------------
// HTTP helpers: session login + the form contracts the console renders
// ---------------------------------------------------------------------------

fn with_peer_ip<B>(mut request: axum::http::Request<B>) -> axum::http::Request<B> {
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            9000,
        ))));
    request
}

/// A logged-in browser: the `lied_session` cookie value.
struct Browser {
    cookie: String,
}

impl Browser {
    /// Log in over HTTP exactly as the login form does (the login POST is
    /// deliberately CSRF-exempt, so no token is needed here).
    async fn login(app: &axum::Router, username: &str) -> Self {
        let body = format!("username={username}&password={PASSWORD}");
        let request = with_peer_ip(
            axum::http::Request::builder()
                .method("POST")
                .uri("/admin/login")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(axum::body::Body::from(body))
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

    fn post_form(&self, uri: &str, body: String) -> axum::http::Request<axum::body::Body> {
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

    fn post_file(
        &self,
        uri: &str,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> axum::http::Request<axum::body::Body> {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        with_peer_ip(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header(axum::http::header::COOKIE, &self.cookie)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
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

/// Fetch a file screen and return `(status, html)`.
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

/// The session's CSRF token as the middleware hands it to the client: the
/// readable `lied_csrf` cookie set on safe requests. Needed for a role that
/// renders no write form (and therefore embeds no token in the HTML) but must
/// still be shown to fail on *authorization*, not on CSRF.
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

/// The CSRF token the page embeds in its multipart form actions (`?csrf=…`) —
/// the query-token half of the double-submit pair that only the streaming
/// upload/replace posts may use.
fn upload_csrf(html: &str) -> String {
    let marker = "?csrf=";
    let start = html.find(marker).expect("page must render an upload form") + marker.len();
    html[start..]
        .split('"')
        .next()
        .expect("token is quoted")
        .to_string()
}

/// The CSRF token the page embeds as a hidden field, used by the urlencoded
/// delete/undelete forms.
fn form_csrf(html: &str) -> String {
    let marker = "name=\"csrf_token\" value=\"";
    let start = html.find(marker).expect("page must render a csrf field") + marker.len();
    html[start..]
        .split('"')
        .next()
        .expect("token is quoted")
        .to_string()
}

async fn audit_count(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar(r#"SELECT count(*) FROM audit_log WHERE action = $1"#)
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn archivist_uploads_replaces_deletes_and_restores_a_score_file() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let files_url = format!("/admin/orgs/{}/arrangements/{}/files", fx.org_id, fx.arr_id);

    // --- upload ---
    let (status, html) = load_page(&ctx.app, &browser, &files_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("No files yet."), "empty state renders");
    let token = upload_csrf(&html);

    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_file(
            &format!("{files_url}?csrf={token}"),
            "score.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .expect("upload runs");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SEE_OTHER,
        "a successful console upload redirects back to the list"
    );

    let (files, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    assert_eq!(files.len(), 1);
    let original = files[0].clone();
    assert_eq!(original.name, "score");
    assert_eq!(original.format, "pdf");
    assert_eq!(audit_count(&ctx.pool, "file.create").await, 1);

    // The list surfaces the transparency columns (format / MIME / size).
    let (_, html) = load_page(&ctx.app, &browser, &files_url).await;
    assert!(html.contains("application/pdf"), "MIME type is shown");
    assert!(
        html.contains(&format!("{} B", PDF.len())),
        "object size is shown"
    );

    // --- replace: new row, old row soft-deleted, same derived key, new bytes ---
    let token = upload_csrf(&html);
    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_file(
            &format!("{files_url}/{}/replace?csrf={token}", original.id),
            "score.pdf",
            "application/pdf",
            PDF_V2,
        ))
        .await
        .expect("replace runs");
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

    let (files, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    assert_eq!(files.len(), 1, "replace leaves exactly one live row");
    let replacement = files[0].clone();
    assert_ne!(
        replacement.id, original.id,
        "atomic replacement inserts a NEW row rather than mutating in place"
    );
    assert!(
        file::find_by_id_including_deleted(&ctx.pool, original.id)
            .await
            .unwrap()
            .expect("old row is retained")
            .deleted_at
            .is_some(),
        "the replaced row is soft-deleted, preserving history"
    );
    assert_eq!(audit_count(&ctx.pool, "file.replace").await, 1);

    let key = file::derived_key(&fx.org_slug, &fx.arr_slug, None, "score", "pdf");
    let head = storage::head_object(&ctx.state.s3, BUCKET, &key)
        .await
        .expect("object lives at the derived key");
    assert_eq!(
        head.size,
        PDF_V2.len() as u64,
        "the replacement bytes are what the derived key now serves"
    );

    // --- soft-delete, then restore from the "previous / deleted versions" list ---
    let (_, html) = load_page(&ctx.app, &browser, &files_url).await;
    let token = form_csrf(&html);
    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_form(
            &format!("{files_url}/{}/delete", replacement.id),
            format!("csrf_token={token}"),
        ))
        .await
        .expect("delete runs");
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let (files, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    assert!(files.is_empty(), "soft-deleted file leaves the live list");
    assert_eq!(audit_count(&ctx.pool, "file.soft_delete").await, 1);

    let (_, html) = load_page(&ctx.app, &browser, &files_url).await;
    assert!(
        html.contains(&format!("{}/undelete", replacement.id)),
        "the deleted file is offered for restore"
    );
    let token = form_csrf(&html);
    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_form(
            &format!("{files_url}/{}/undelete", replacement.id),
            format!("csrf_token={token}"),
        ))
        .await
        .expect("undelete runs");
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let (files, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    assert_eq!(files.len(), 1, "restore brings the file back");
    assert_eq!(audit_count(&ctx.pool, "file.undelete").await, 1);
}

#[tokio::test]
async fn voice_file_upload_lands_under_the_voice_prefix_and_a_hostile_name_cannot_escape_it() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let files_url = format!(
        "/admin/orgs/{}/arrangements/{}/voices/{}/files",
        fx.org_id, fx.arr_id, fx.voice_id
    );

    let (status, html) = load_page(&ctx.app, &browser, &files_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let token = upload_csrf(&html);

    // A filename with directory components must not inject path segments into
    // the derived storage key.
    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_file(
            &format!("{files_url}?csrf={token}"),
            "../../etc/passwd.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .expect("upload runs");
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

    let (files, _) = file::list(&ctx.pool, fx.arr_id, Some(fx.voice_id), 50, 0)
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].name, "passwd",
        "directory components are stripped from the stored name"
    );

    let key = file::derived_key(
        &fx.org_slug,
        &fx.arr_slug,
        Some(&fx.voice_slug),
        "passwd",
        "pdf",
    );
    assert_eq!(
        key,
        format!(
            "orgs/{}/arrangements/{}/voices/{}/passwd.pdf",
            fx.org_slug, fx.arr_slug, fx.voice_slug
        ),
        "voice files live under the voice prefix"
    );
    storage::head_object(&ctx.state.s3, BUCKET, &key)
        .await
        .expect("object stored inside the voice prefix, not above it");
}

#[tokio::test]
async fn conductor_sees_the_list_read_only_and_a_musician_is_denied() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let fx = seed(&ctx.state).await;
    let files_url = format!("/admin/orgs/{}/arrangements/{}/files", fx.org_id, fx.arr_id);

    // Seed one file through the archivist so the conductor has a row to see.
    let archivist = Browser::login(&ctx.app, "arch").await;
    let (_, html) = load_page(&ctx.app, &archivist, &files_url).await;
    let token = upload_csrf(&html);
    ctx.app
        .clone()
        .oneshot(archivist.post_file(
            &format!("{files_url}?csrf={token}"),
            "score.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .expect("seed upload runs");

    // Conductor: reads the list, but gets no write affordances…
    let conductor = Browser::login(&ctx.app, "cond").await;
    let (status, html) = load_page(&ctx.app, &conductor, &files_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("read-only access"), "read-only notice shown");
    assert!(html.contains("application/pdf"), "the file row is visible");
    assert!(
        !html.contains("enctype=\"multipart/form-data\""),
        "no upload/replace form for a conductor"
    );
    assert!(
        !html.contains("/delete\""),
        "no delete button for a conductor"
    );

    // …and the write route itself refuses, not just the rendering. The token
    // is this conductor's own valid one, so the rejection is authorization —
    // not the CSRF check standing in for it.
    let token = session_csrf(&ctx.app, &conductor, &files_url).await;
    let response = ctx
        .app
        .clone()
        .oneshot(conductor.post_file(
            &format!("{files_url}?csrf={token}"),
            "sneak.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .expect("conductor upload runs");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "a conductor must not be able to upload"
    );
    let (files, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    assert_eq!(files.len(), 1, "the conductor's upload stored nothing");

    // Plain musician: no console access to the file screens at all.
    let musician = Browser::login(&ctx.app, "mus").await;
    let (status, _) = load_page(&ctx.app, &musician, &files_url).await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "a plain musician is denied the file screens"
    );
}

#[tokio::test]
async fn a_multipart_upload_without_the_csrf_query_token_is_rejected() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let files_url = format!("/admin/orgs/{}/arrangements/{}/files", fx.org_id, fx.arr_id);
    // Establish the session's token (rendering the page mints it).
    let (_, html) = load_page(&ctx.app, &browser, &files_url).await;
    let good = upload_csrf(&html);

    for uri in [files_url.clone(), format!("{files_url}?csrf=not-the-token")] {
        let response = ctx
            .app
            .clone()
            .oneshot(browser.post_file(&uri, "score.pdf", "application/pdf", PDF))
            .await
            .expect("upload runs");
        assert_eq!(
            response.status(),
            axum::http::StatusCode::FORBIDDEN,
            "upload to {uri} must fail the CSRF check"
        );
    }

    let (files, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    assert!(files.is_empty(), "no file was stored by a rejected upload");

    // Control: the same request with the real token succeeds, so the rejections
    // above are the CSRF check and not some unrelated failure.
    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_file(
            &format!("{files_url}?csrf={good}"),
            "score.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .expect("upload runs");
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

    // The query-token escape hatch exists *only* because a multipart body
    // can't carry a form field. An ordinary urlencoded form must still put the
    // token in its body — a valid token in the URL must not authorize it, so
    // admin tokens never leak into URLs (referrers, history, access logs).
    let (files, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    let file_id = files[0].id;
    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_form(
            &format!("{files_url}/{file_id}/delete?csrf={good}"),
            String::new(),
        ))
        .await
        .expect("delete runs");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "a urlencoded form must not be authorized by a query-string token"
    );
    let (files, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    assert_eq!(files.len(), 1, "the rejected delete changed nothing");
}

#[tokio::test]
async fn unsupported_format_and_oversize_uploads_render_friendly_errors() {
    // Cap the upload size low enough that a tiny PDF trips it, while the
    // unsupported-format probe stays under the cap (so it fails on format).
    let ctx = Ctx::new(16).await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let files_url = format!("/admin/orgs/{}/arrangements/{}/files", fx.org_id, fx.arr_id);
    let (_, html) = load_page(&ctx.app, &browser, &files_url).await;
    let token = upload_csrf(&html);

    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_file(
            &format!("{files_url}?csrf={token}"),
            "notes.txt",
            "text/plain",
            b"tiny",
        ))
        .await
        .expect("upload runs");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let html = body_text(response).await;
    assert!(
        html.contains("not a stored format"),
        "415 page explains which formats are accepted, got: {html}"
    );

    let response = ctx
        .app
        .clone()
        .oneshot(browser.post_file(
            &format!("{files_url}?csrf={token}"),
            "score.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .expect("upload runs");
    assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    let html = body_text(response).await;
    assert!(
        html.contains("too large"),
        "413 page names the limit, got: {html}"
    );

    // A rejected upload must leave neither a phantom row nor a stored object:
    // the rollback hard-deletes the row it optimistically created.
    let (live, _) = file::list(&ctx.pool, fx.arr_id, None, 50, 0).await.unwrap();
    assert!(live.is_empty(), "no live row from a failed upload");
    assert!(
        file::list_deleted(&ctx.pool, fx.arr_id, None)
            .await
            .unwrap()
            .is_empty(),
        "no restorable phantom row from a failed upload"
    );
    let key = file::derived_key(&fx.org_slug, &fx.arr_slug, None, "score", "pdf");
    assert!(
        storage::head_object(&ctx.state.s3, BUCKET, &key)
            .await
            .is_err(),
        "no object written for an over-limit upload"
    );
}
