//! Integration tests for issue #7 (File upload / download + MinIO streaming).
//! Spins a real Postgres **and** a real MinIO via `testcontainers`, drives the
//! axum router with `tower::ServiceExt::oneshot`, and asserts the acceptance
//! criteria end to end:
//!   - upload to a voice and to a full score, stored at the derived key;
//!   - download returns byte-identical content; `Range` → 206 partial;
//!   - oversize upload → 413; unsupported mime → 415;
//!   - replace inserts a new row + soft-deletes the old, chain intact;
//!   - cross-org access → 404; every write audited.

mod common;

use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{arrangement, membership, organization, user, voice};
use lied::state::AppState;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::GenericImage;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

const BUCKET: &str = "lied-test";
const BOUNDARY: &str = "X-LIED-TEST-BOUNDARY";

/// Holds both containers (kept alive for the test) plus the wired app/state.
struct Ctx {
    _pg: ContainerAsync<Postgres>,
    _store: ContainerAsync<GenericImage>,
    pool: PgPool,
    app: axum::Router,
    state: AppState,
}

impl Ctx {
    async fn new(max_upload_bytes: u64) -> Self {
        Self::build(max_upload_bytes, 50).await
    }

    async fn build(max_upload_bytes: u64, max_files_per_voice: u32) -> Self {
        // --- Postgres ---
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

        // --- MinIO ---
        // Object storage: the pinned RustFS image (tests/common).
        let (store, endpoint) = common::start_object_store().await;

        let config = test_config(endpoint, max_upload_bytes, max_files_per_voice);
        let state = test_state(pool.clone(), config);

        // Create the bucket the app will write to.
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
            _store: store,
            pool,
            app,
            state,
        }
    }
}

fn test_config(s3_endpoint: String, max_upload_bytes: u64, max_files_per_voice: u32) -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint,
        s3_bucket: BUCKET.to_string(),
        s3_access_key_id: Secret::from(common::ACCESS_KEY.to_string()),
        s3_secret_access_key: Secret::from(common::SECRET_KEY.to_string()),
        s3_region: "us-east-1".to_string(),
        s3_version_retention_days: 30,
        jwt_signing_key: Secret::from("test-signing-key-material-for-file-tests".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes,
        max_request_bytes: 256 * 1024,
        max_files_per_voice,
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

// --- fixtures (via the domain layer, like tests/arrangements.rs) ---

async fn seed_org_archivist(state: &AppState, name: &str, username: &str) -> (Uuid, String) {
    let org_id = Uuid::now_v7();
    let slug = organization::slugify(name);
    organization::create(&state.db, org_id, name, &slug, None)
        .await
        .expect("org");
    let hash = auth::password::hash_password("pw").expect("hash");
    let uid = Uuid::now_v7();
    user::create(
        &state.db,
        uid,
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
    membership::create(
        &state.db,
        Uuid::now_v7(),
        uid,
        org_id,
        membership::MembershipFields {
            role: membership::Role::Archivist,
            instrument_ids: &[],
            is_principal: false,
            principal_instrument_ids: &[],
        },
        None,
    )
    .await
    .expect("membership");
    let (token, _) = state.jwt_keyring.mint(uid, None).expect("token");
    (org_id, token)
}

async fn seed_arrangement(state: &AppState, org_id: Uuid, title: &str) -> Uuid {
    let id = Uuid::now_v7();
    arrangement::create(
        &state.db,
        id,
        org_id,
        &arrangement::slugify(title),
        arrangement::ArrangementFields {
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
        },
        None,
    )
    .await
    .expect("arrangement");
    id
}

async fn seed_voice(state: &AppState, arr_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    let instrument_id = sqlx::query_scalar!(r#"SELECT id FROM instrument ORDER BY key LIMIT 1"#)
        .fetch_one(&state.db)
        .await
        .expect("instrument");
    voice::create(
        &state.db,
        id,
        arr_id,
        name,
        &voice::slugify(name),
        &[instrument_id],
        lied::domain::voice::NewPartNumber::Set(None),
        None,
    )
    .await
    .expect("voice");
    id
}

// --- HTTP helpers ---

fn multipart_upload(
    uri: &str,
    token: &str,
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

    axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(
            axum::http::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(axum::body::Body::from(body))
        .unwrap()
}

fn get_request(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
}

async fn audit_count(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar(r#"SELECT count(*) FROM audit_log WHERE action = $1"#)
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

const PDF: &[u8] = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n";

// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_to_a_voice_then_download_roundtrips_bytes_and_audits() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Overture").await;
    let voice_id = seed_voice(&ctx.state, arr_id, "Flute 1").await;

    let up = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}/files"),
            &token,
            "part.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .unwrap();
    assert_eq!(up.status(), axum::http::StatusCode::CREATED);
    let created = body_json(up).await;
    assert_eq!(created["name"], "part");
    assert_eq!(created["format"], "pdf");
    assert_eq!(created["voiceId"], voice_id.to_string());
    let file_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let down = ctx
        .app
        .clone()
        .oneshot(get_request(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}/files/{file_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(down.status(), axum::http::StatusCode::OK);
    assert_eq!(
        down.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap(),
        "application/pdf"
    );
    assert_eq!(body_bytes(down).await, PDF);
    assert_eq!(audit_count(&ctx.pool, "file.create").await, 1);
}

#[tokio::test]
async fn upload_to_a_full_score_and_download() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Symphony").await;

    let up = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files"),
            &token,
            "full-score.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .unwrap();
    assert_eq!(up.status(), axum::http::StatusCode::CREATED);
    let created = body_json(up).await;
    assert!(created["voiceId"].is_null());
    let file_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let down = ctx
        .app
        .clone()
        .oneshot(get_request(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files/{file_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(down.status(), axum::http::StatusCode::OK);
    assert_eq!(body_bytes(down).await, PDF);
}

#[tokio::test]
async fn download_honors_range_requests() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Overture").await;

    let up = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files"),
            &token,
            "score.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .unwrap();
    let file_id = Uuid::parse_str(body_json(up).await["id"].as_str().unwrap()).unwrap();

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!(
            "/v1/orgs/{org_id}/arrangements/{arr_id}/files/{file_id}"
        ))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::RANGE, "bytes=0-7")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::PARTIAL_CONTENT);
    assert!(resp
        .headers()
        .get(axum::http::header::CONTENT_RANGE)
        .is_some());
    assert_eq!(body_bytes(resp).await, &PDF[0..8]);
}

#[tokio::test]
async fn oversize_upload_is_rejected_with_413() {
    let ctx = Ctx::new(8).await; // 8-byte ceiling
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Overture").await;

    let resp = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files"),
            &token,
            "score.pdf",
            "application/pdf",
            PDF, // > 8 bytes
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);

    // The orphaned DB row was rolled back (soft-deleted), so a re-upload of
    // the same name does not 409.
    let live: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM file WHERE arrangement_id = $1 AND deleted_at IS NULL"#,
    )
    .bind(arr_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(live, 0);
}

#[tokio::test]
async fn unsupported_mime_is_rejected_with_415() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Overture").await;

    let resp = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files"),
            &token,
            "evil.zip",
            "application/zip",
            b"PK\x03\x04",
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[tokio::test]
async fn replacing_a_file_creates_new_row_soft_deletes_old_and_serves_new_bytes() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Overture").await;

    let up = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files"),
            &token,
            "score.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .unwrap();
    let old_id = Uuid::parse_str(body_json(up).await["id"].as_str().unwrap()).unwrap();

    let new_bytes = b"%PDF-1.7\nREPLACED CONTENT\n%%EOF\n";
    let req = multipart_upload(
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files/{old_id}"),
        &token,
        "score.pdf",
        "application/pdf",
        new_bytes,
    );
    // Same builder, but the verb must be PUT.
    let (mut parts, body) = req.into_parts();
    parts.method = axum::http::Method::PUT;
    let resp = ctx
        .app
        .clone()
        .oneshot(axum::http::Request::from_parts(parts, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let new_file = body_json(resp).await;
    let new_id = Uuid::parse_str(new_file["id"].as_str().unwrap()).unwrap();
    assert_ne!(new_id, old_id);

    // Old row soft-deleted, exactly one live row remains.
    let old_deleted: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar(r#"SELECT deleted_at FROM file WHERE id = $1"#)
            .bind(old_id)
            .fetch_one(&ctx.pool)
            .await
            .unwrap();
    assert!(old_deleted.is_some(), "old row should be soft-deleted");
    let live: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM file WHERE arrangement_id = $1 AND deleted_at IS NULL"#,
    )
    .bind(arr_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(live, 1);

    // Download serves the new bytes.
    let down = ctx
        .app
        .clone()
        .oneshot(get_request(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files/{new_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(body_bytes(down).await, new_bytes);
    assert_eq!(audit_count(&ctx.pool, "file.replace").await, 1);
}

#[tokio::test]
async fn a_file_is_not_reachable_through_another_orgs_path() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_a, token_a) = seed_org_archivist(&ctx.state, "Org A", "arch_a").await;
    let arr_id = seed_arrangement(&ctx.state, org_a, "Org A Piece").await;
    let up = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_a}/arrangements/{arr_id}/files"),
            &token_a,
            "secret.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .unwrap();
    let file_id = Uuid::parse_str(body_json(up).await["id"].as_str().unwrap()).unwrap();

    let (org_b, token_b) = seed_org_archivist(&ctx.state, "Org B", "arch_b").await;
    let resp = ctx
        .app
        .clone()
        .oneshot(get_request(
            &format!("/v1/orgs/{org_b}/arrangements/{arr_id}/files/{file_id}"),
            &token_b,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn replacing_with_a_different_format_is_rejected() {
    // Regression: replace must keep the original's format, otherwise the bytes
    // (e.g. PNG) would be stored under a PDF identity at the `.pdf` key.
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Suite").await;

    let up = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files"),
            &token,
            "score.pdf",
            "application/pdf",
            PDF,
        ))
        .await
        .unwrap();
    let file_id = Uuid::parse_str(body_json(up).await["id"].as_str().unwrap()).unwrap();

    // PUT a PNG over the PDF → 409 Conflict (format mismatch).
    let req = multipart_upload(
        &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files/{file_id}"),
        &token,
        "score.png",
        "image/png",
        b"\x89PNG\r\n\x1a\n",
    );
    let (mut parts, body) = req.into_parts();
    parts.method = axum::http::Method::PUT;
    let resp = ctx
        .app
        .clone()
        .oneshot(axum::http::Request::from_parts(parts, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);

    // The original PDF is untouched and still downloads byte-identically.
    let down = ctx
        .app
        .clone()
        .oneshot(get_request(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files/{file_id}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(down.status(), axum::http::StatusCode::OK);
    assert_eq!(body_bytes(down).await, PDF);
}

#[tokio::test]
async fn an_unsatisfiable_range_returns_416() {
    // Regression: a Range outside the object must 416, not 500.
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Suite").await;

    let up = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            &format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files"),
            &token,
            "score.pdf",
            "application/pdf",
            PDF, // ~47 bytes
        ))
        .await
        .unwrap();
    let file_id = Uuid::parse_str(body_json(up).await["id"].as_str().unwrap()).unwrap();

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!(
            "/v1/orgs/{org_id}/arrangements/{arr_id}/files/{file_id}"
        ))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::RANGE, "bytes=100000-200000")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::RANGE_NOT_SATISFIABLE);
}

/// `LIED_MAX_FILES_PER_VOICE` on `/v1` (#45), at a low configured ceiling.
/// The full score is capped like a voice; each bucket counts on its own; a
/// replace at the ceiling succeeds; a soft-deleted file frees its place.
#[tokio::test]
async fn a_full_score_or_voice_refuses_new_files_with_409() {
    let ctx = Ctx::build(200 * 1024 * 1024, 2).await;
    let (org_id, token) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Suite").await;
    let voice_id = seed_voice(&ctx.state, arr_id, "Flute 1").await;
    let score = format!("/v1/orgs/{org_id}/arrangements/{arr_id}/files");
    let voice = format!("/v1/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}/files");
    let send = |request: axum::http::Request<axum::body::Body>| {
        let app = ctx.app.clone();
        async move { app.oneshot(request).await.unwrap() }
    };
    let upload =
        |uri: &str, name: &str| multipart_upload(uri, &token, name, "application/pdf", PDF);

    let first = send(upload(&score, "a.pdf")).await;
    assert_eq!(first.status(), axum::http::StatusCode::CREATED);
    let a = body_json(first).await["id"].as_str().unwrap().to_string();
    let second = send(upload(&score, "b.pdf")).await;
    assert_eq!(second.status(), axum::http::StatusCode::CREATED);
    let b = body_json(second).await["id"].as_str().unwrap().to_string();

    let refused = send(upload(&score, "c.pdf")).await;
    assert_eq!(refused.status(), axum::http::StatusCode::CONFLICT);
    let problem = body_json(refused).await;
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("maximum of 2 files"),
        "{problem}"
    );
    assert_eq!(audit_count(&ctx.pool, "file.create").await, 2);

    // Another bucket is not affected by the full score.
    let in_voice = send(upload(&voice, "part.pdf")).await;
    assert_eq!(in_voice.status(), axum::http::StatusCode::CREATED);

    // A replace at the ceiling is an identity swap and goes through.
    let (mut parts, body) = upload(&format!("{score}/{a}"), "a.pdf").into_parts();
    parts.method = axum::http::Method::PUT;
    let replaced = send(axum::http::Request::from_parts(parts, body)).await;
    assert_eq!(replaced.status(), axum::http::StatusCode::OK);

    // Soft-deleting b frees its place.
    let delete = axum::http::Request::builder()
        .method("DELETE")
        .uri(format!("{score}/{b}"))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(
        send(delete).await.status(),
        axum::http::StatusCode::NO_CONTENT
    );
    let after = send(upload(&score, "c.pdf")).await;
    assert_eq!(after.status(), axum::http::StatusCode::CREATED);
}

/// Concurrent creates cannot overshoot the ceiling: the count and the insert
/// are one step per voice (#45).
#[tokio::test]
async fn concurrent_creates_stop_at_the_ceiling() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, _) = seed_org_archivist(&ctx.state, "Org", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Suite").await;
    let voice_id = seed_voice(&ctx.state, arr_id, "Flute 1").await;
    let names: Vec<String> = (0..6).map(|n| format!("part-{n}")).collect();
    let creates = names.iter().map(|name| {
        lied::domain::file::create(
            &ctx.pool,
            Uuid::now_v7(),
            lied::domain::file::NewFile {
                arrangement_id: arr_id,
                voice_id: Some(voice_id),
                name,
                format: "pdf",
                mime_type: "application/pdf",
                derived_from_file_id: None,
                conversion_quality: None,
                created_by: None,
            },
            2,
        )
    });
    let results = futures_util::future::join_all(creates).await;
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        2,
        "{results:?}"
    );
    assert!(results.iter().all(|r| matches!(
        r,
        Ok(_) | Err(lied::domain::file::FileError::TooManyFiles { limit: 2 })
    )));
}

// --- #99: guests read their assigned parts through /v1 ---

/// A user with no membership anywhere, and a bearer token for them.
async fn seed_guest(state: &AppState, username: &str) -> (Uuid, String) {
    let uid = Uuid::now_v7();
    user::create(
        &state.db,
        uid,
        &user::slugify(username),
        username,
        None,
        None,
        username,
        false,
        None,
    )
    .await
    .expect("guest user");
    let (token, _) = state.jwt_keyring.mint(uid, None).expect("token");
    (uid, token)
}

/// A program collection holding `arr_id` as its only piece: (collection, item).
async fn seed_program_item(state: &AppState, org_id: Uuid, arr_id: Uuid) -> (Uuid, Uuid) {
    let collection_id = Uuid::now_v7();
    lied::domain::collection::create(
        &state.db,
        collection_id,
        org_id,
        "Spring concert",
        &format!("spring-{collection_id}"),
        "program",
        3,
        None,
    )
    .await
    .expect("collection");
    let item = lied::domain::collection_item::create(
        &state.db,
        Uuid::now_v7(),
        collection_id,
        arr_id,
        lied::domain::collection_item::Placement {
            position: None,
            label: None,
        },
        None,
    )
    .await
    .expect("item");
    (collection_id, item.id)
}

async fn assign(state: &AppState, item_id: Uuid, voice_id: Uuid, user_id: Uuid) -> Uuid {
    lied::domain::part_assignment::assign(
        &state.db,
        Uuid::now_v7(),
        item_id,
        voice_id,
        user_id,
        None,
    )
    .await
    .expect("assignment")
    .id
}

/// Upload a PDF as `token` to `uri` (a `…/files` collection) and return its id.
async fn upload_pdf(ctx: &Ctx, token: &str, uri: &str, bytes: &[u8]) -> Uuid {
    let up = ctx
        .app
        .clone()
        .oneshot(multipart_upload(
            uri,
            token,
            "part.pdf",
            "application/pdf",
            bytes,
        ))
        .await
        .unwrap();
    assert_eq!(up.status(), axum::http::StatusCode::CREATED, "{uri}");
    Uuid::parse_str(body_json(up).await["id"].as_str().unwrap()).unwrap()
}

async fn status_of(ctx: &Ctx, request: axum::http::Request<axum::body::Body>) -> u16 {
    ctx.app
        .clone()
        .oneshot(request)
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn json_request(
    method: &str,
    uri: &str,
    token: &str,
    body: serde_json::Value,
) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::IF_MATCH, "\"0\"")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

/// The guest's statuses for the arrangement, the voice and the file.
async fn guest_reads(ctx: &Ctx, token: &str, uris: &[&str]) -> Vec<u16> {
    let mut statuses = Vec::new();
    for uri in uris {
        statuses.push(status_of(ctx, get_request(uri, token)).await);
    }
    statuses
}

/// A guest (a part assignment, no membership) reads exactly what the
/// assignment grants over `/v1`: the arrangement's summary, the assigned
/// voice, its files (with `Range`). Everything else is 403, existing or not,
/// so the org's other repertoire stays hidden; nothing can be written.
#[tokio::test]
async fn a_guest_reads_only_the_assigned_part_through_v1() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, arch) = seed_org_archivist(&ctx.state, "Stadtkapelle", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Radetzky March").await;
    sqlx::query(
        "UPDATE arrangement SET publisher = 'Edition X', license_notes = 'two copies',
         arranger = 'A. Arranger' WHERE id = $1",
    )
    .bind(arr_id)
    .execute(&ctx.pool)
    .await
    .unwrap();
    let flute = seed_voice(&ctx.state, arr_id, "Flute 1").await;
    let oboe = seed_voice(&ctx.state, arr_id, "Oboe 1").await;
    let other_arr = seed_arrangement(&ctx.state, org_id, "Florentiner").await;
    let base = format!("/v1/orgs/{org_id}/arrangements/{arr_id}");
    let score_file = upload_pdf(&ctx, &arch, &format!("{base}/files"), PDF).await;
    let flute_file = upload_pdf(&ctx, &arch, &format!("{base}/voices/{flute}/files"), PDF).await;
    let oboe_file = upload_pdf(&ctx, &arch, &format!("{base}/voices/{oboe}/files"), PDF).await;

    let (_collection, item) = seed_program_item(&ctx.state, org_id, arr_id).await;
    let (guest_id, guest) = seed_guest(&ctx.state, "anna").await;
    assign(&ctx.state, item, flute, guest_id).await;

    // The arrangement's summary: musical fields as stored, licensing nulled.
    let response = ctx
        .app
        .clone()
        .oneshot(get_request(&base, &guest))
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(response.headers().get(axum::http::header::ETAG).is_some());
    let summary = body_json(response).await;
    assert_eq!(summary["title"], "Radetzky March");
    assert_eq!(summary["arranger"], "A. Arranger");
    assert!(summary["publisher"].is_null(), "{summary}");
    assert!(summary["licenseNotes"].is_null(), "{summary}");
    // A member still sees everything.
    let full = body_json(
        ctx.app
            .clone()
            .oneshot(get_request(&base, &arch))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(full["publisher"], "Edition X");

    // The assigned voice and its files, Range included.
    assert_eq!(
        status_of(&ctx, get_request(&format!("{base}/voices/{flute}"), &guest)).await,
        200
    );
    let list = body_json(
        ctx.app
            .clone()
            .oneshot(get_request(&format!("{base}/voices/{flute}/files"), &guest))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list["total"], 1, "{list}");
    let file_uri = format!("{base}/voices/{flute}/files/{flute_file}");
    let whole = ctx
        .app
        .clone()
        .oneshot(get_request(&file_uri, &guest))
        .await
        .unwrap();
    assert_eq!(whole.status(), axum::http::StatusCode::OK);
    assert_eq!(body_bytes(whole).await, PDF);
    let mut ranged = get_request(&file_uri, &guest);
    ranged.headers_mut().insert(
        axum::http::header::RANGE,
        axum::http::HeaderValue::from_static("bytes=0-3"),
    );
    let partial = ctx.app.clone().oneshot(ranged).await.unwrap();
    assert_eq!(partial.status(), axum::http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(partial).await, b"%PDF");

    // Nothing else: other voice, the score, lists, other pieces, made-up ids.
    let unknown = Uuid::now_v7();
    for uri in [
        format!("{base}/voices/{oboe}"),
        format!("{base}/voices/{oboe}/files"),
        format!("{base}/voices/{oboe}/files/{oboe_file}"),
        format!("{base}/files"),
        format!("{base}/files/{score_file}"),
        format!("{base}/voices"),
        format!("/v1/orgs/{org_id}/arrangements"),
        format!("/v1/orgs/{org_id}/arrangements/{other_arr}"),
        format!("/v1/orgs/{org_id}/arrangements/{unknown}"),
        format!("{base}/voices/{unknown}"),
        format!("/v1/orgs/{org_id}/arrangements/{other_arr}/voices/{flute}"),
        format!("/v1/orgs/{org_id}/members"),
        format!("/v1/orgs/{org_id}/collections"),
    ] {
        assert_eq!(
            status_of(&ctx, get_request(&uri, &guest)).await,
            403,
            "{uri}"
        );
    }

    // And no writes.
    assert_eq!(
        status_of(
            &ctx,
            json_request("PATCH", &base, &guest, serde_json::json!({ "title": "x" }))
        )
        .await,
        403
    );
    assert_eq!(
        status_of(
            &ctx,
            json_request(
                "PATCH",
                &format!("{base}/voices/{flute}"),
                &guest,
                serde_json::json!({ "name": "x" })
            )
        )
        .await,
        403
    );
    assert_eq!(
        status_of(
            &ctx,
            multipart_upload(
                &format!("{base}/voices/{flute}/files"),
                &guest,
                "mine.pdf",
                "application/pdf",
                PDF
            )
        )
        .await,
        403
    );
}

/// The access lasts exactly as long as a live assignment: deleting the
/// assignment, removing the piece from the collection, or deleting the
/// collection ends it.
#[tokio::test]
async fn a_guests_access_ends_with_the_assignment_or_its_collection() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (org_id, arch) = seed_org_archivist(&ctx.state, "Stadtkapelle", "arch").await;
    let arr_id = seed_arrangement(&ctx.state, org_id, "Radetzky March").await;
    let flute = seed_voice(&ctx.state, arr_id, "Flute 1").await;
    let base = format!("/v1/orgs/{org_id}/arrangements/{arr_id}");
    let file = upload_pdf(&ctx, &arch, &format!("{base}/voices/{flute}/files"), PDF).await;
    let (guest_id, guest) = seed_guest(&ctx.state, "anna").await;
    let voice_uri = format!("{base}/voices/{flute}");
    let file_uri = format!("{voice_uri}/files/{file}");
    let uris = [base.as_str(), voice_uri.as_str(), file_uri.as_str()];

    // 1. Deleting the assignment.
    let (collection, item) = seed_program_item(&ctx.state, org_id, arr_id).await;
    let assignment = assign(&ctx.state, item, flute, guest_id).await;
    assert_eq!(guest_reads(&ctx, &guest, &uris).await, [200, 200, 200]);
    assert!(
        lied::domain::part_assignment::delete(&ctx.state.db, assignment, None)
            .await
            .unwrap()
    );
    assert_eq!(guest_reads(&ctx, &guest, &uris).await, [403, 403, 403]);

    // 2. Removing the piece from the collection.
    assign(&ctx.state, item, flute, guest_id).await;
    assert_eq!(guest_reads(&ctx, &guest, &uris).await, [200, 200, 200]);
    assert!(
        lied::domain::collection_item::remove(&ctx.state.db, collection, item, None)
            .await
            .unwrap()
    );
    assert_eq!(guest_reads(&ctx, &guest, &uris).await, [403, 403, 403]);

    // 3. Deleting the collection.
    let (collection, item) = seed_program_item(&ctx.state, org_id, arr_id).await;
    assign(&ctx.state, item, flute, guest_id).await;
    assert_eq!(guest_reads(&ctx, &guest, &uris).await, [200, 200, 200]);
    assert!(
        lied::domain::collection::soft_delete(&ctx.state.db, collection, None)
            .await
            .unwrap()
    );
    assert_eq!(guest_reads(&ctx, &guest, &uris).await, [403, 403, 403]);
}

// --- #98: GET /v1/me/parts ---

async fn my_parts(ctx: &Ctx, token: &str, query: &str) -> (u16, serde_json::Value) {
    let response = ctx
        .app
        .clone()
        .oneshot(get_request(&format!("/v1/me/parts{query}"), token))
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, body_json(response).await)
}

/// The caller's parts across every ensemble, their own and those they play
/// in as a guest; filtered and sorted by the allowlist; only live ones; and
/// every listed part opens through the voice endpoint.
#[tokio::test]
async fn my_parts_list_every_live_part_across_ensembles() {
    let ctx = Ctx::new(200 * 1024 * 1024).await;
    let (home, _) = seed_org_archivist(&ctx.state, "Stadtkapelle", "arch").await;
    let (away, _) = seed_org_archivist(&ctx.state, "Abendchor", "arch2").await;
    let (anna, token) = seed_guest(&ctx.state, "anna").await;
    let (bob, _) = seed_guest(&ctx.state, "bob").await;
    // Anna is a musician at home, and a guest in the other ensemble.
    membership::create(
        &ctx.state.db,
        Uuid::now_v7(),
        anna,
        home,
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

    let radetzky = seed_arrangement(&ctx.state, home, "Radetzky March").await;
    let flute = seed_voice(&ctx.state, radetzky, "Flute 1").await;
    let (_home_collection, home_item) = seed_program_item(&ctx.state, home, radetzky).await;
    assign(&ctx.state, home_item, flute, anna).await;
    assign(&ctx.state, home_item, flute, bob).await; // not Anna's

    let florentiner = seed_arrangement(&ctx.state, away, "Florentiner").await;
    let clarinet = seed_voice(&ctx.state, florentiner, "Clarinet 1").await;
    let oboe = seed_voice(&ctx.state, florentiner, "Oboe 1").await;
    let (_concert, concert_item) = seed_program_item(&ctx.state, away, florentiner).await;
    let concert_part = assign(&ctx.state, concert_item, clarinet, anna).await;
    let (festival, festival_item) = seed_program_item(&ctx.state, away, florentiner).await;
    assign(&ctx.state, festival_item, oboe, anna).await;

    // Everything, in the default order: by organization name.
    let (status, page) = my_parts(&ctx, &token, "").await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["total"], 3, "{page}");
    let orgs: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["organization"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(orgs, ["Abendchor", "Abendchor", "Stadtkapelle"]);
    let home_row = &page["items"][2];
    assert_eq!(home_row["organization"]["slug"], "stadtkapelle");
    assert_eq!(home_row["collection"]["type"], "program");
    assert_eq!(home_row["item"]["label"], "1");
    assert_eq!(home_row["item"]["position"], 1);
    assert_eq!(home_row["arrangement"]["title"], "Radetzky March");
    assert_eq!(home_row["voice"]["id"], flute.to_string());
    assert_eq!(
        home_row["voice"]["instrumentIds"].as_array().unwrap().len(),
        1
    );
    assert!(home_row["acknowledgedAt"].is_null());

    // Every listed part opens through the voice endpoint, guest parts too.
    for part in page["items"].as_array().unwrap() {
        let uri = format!(
            "/v1/orgs/{}/arrangements/{}/voices/{}",
            part["organization"]["id"].as_str().unwrap(),
            part["arrangement"]["id"].as_str().unwrap(),
            part["voice"]["id"].as_str().unwrap()
        );
        assert_eq!(
            status_of(&ctx, get_request(&uri, &token)).await,
            200,
            "{uri}"
        );
    }

    // Filters.
    let (_, only_home) = my_parts(&ctx, &token, &format!("?filter[organizationId]={home}")).await;
    assert_eq!(only_home["total"], 1, "{only_home}");
    let (_, only_festival) =
        my_parts(&ctx, &token, &format!("?filter[collectionId]={festival}")).await;
    assert_eq!(only_festival["total"], 1);
    assert_eq!(only_festival["items"][0]["voice"]["id"], oboe.to_string());
    lied::domain::part_assignment::update_state(
        &ctx.state.db,
        concert_part,
        None,
        lied::domain::Change::Keep,
        lied::domain::Change::Set(Some(chrono::Utc::now())),
    )
    .await
    .unwrap()
    .expect("acknowledged");
    let (_, acknowledged) = my_parts(&ctx, &token, "?filter[acknowledged]=true").await;
    assert_eq!(acknowledged["total"], 1);
    assert_eq!(
        acknowledged["items"][0]["voice"]["id"],
        clarinet.to_string()
    );
    let (_, open) = my_parts(&ctx, &token, "?filter[acknowledged]=false").await;
    assert_eq!(open["total"], 2);
    let (status, body) = my_parts(&ctx, &token, "?filter[acknowledged]=yes").await;
    assert_eq!(status, 400);
    assert!(
        body["errors"].get("filter[acknowledged]").is_some(),
        "{body}"
    );
    let (status, _) = my_parts(&ctx, &token, "?filter[bogus]=1").await;
    assert_eq!(status, 400);

    // Sort and paginate.
    let (status, by_ack) = my_parts(&ctx, &token, "?sort=acknowledgedAt:desc").await;
    assert_eq!(status, 200);
    // Unset times sort last in either direction.
    assert_eq!(by_ack["items"][0]["voice"]["id"], clarinet.to_string());
    let (_, by_ack_asc) = my_parts(&ctx, &token, "?sort=acknowledgedAt").await;
    assert_eq!(by_ack_asc["items"][0]["voice"]["id"], clarinet.to_string());
    let (status, _) = my_parts(&ctx, &token, "?sort=title").await;
    assert_eq!(status, 400);
    let (_, first) = my_parts(&ctx, &token, "?limit=1").await;
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert_eq!(first["total"], 3);

    // A deleted collection takes its parts with it.
    assert!(
        lied::domain::collection::soft_delete(&ctx.state.db, festival, None)
            .await
            .unwrap()
    );
    let (_, after) = my_parts(&ctx, &token, "").await;
    assert_eq!(after["total"], 2, "{after}");
    let rows = after["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{after}");
    assert!(
        rows.iter()
            .all(|p| p["collection"]["id"] != festival.to_string()),
        "{after}"
    );

    // Without credentials: 401.
    let anonymous = axum::http::Request::builder()
        .uri("/v1/me/parts")
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(status_of(&ctx, anonymous).await, 401);
}
