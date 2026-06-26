//! Integration tests for issue #7 (File upload / download + MinIO streaming).
//! Spins a real Postgres **and** a real MinIO via `testcontainers`, drives the
//! axum router with `tower::ServiceExt::oneshot`, and asserts the acceptance
//! criteria end to end:
//!   - upload to a voice and to a full score, stored at the derived key;
//!   - download returns byte-identical content; `Range` → 206 partial;
//!   - oversize upload → 413; unsupported mime → 415;
//!   - replace inserts a new row + soft-deletes the old, chain intact;
//!   - cross-org access → 404; every write audited.

use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{arrangement, membership, organization, user, voice};
use lied::state::AppState;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

const BUCKET: &str = "lied-test";
const BOUNDARY: &str = "X-LIED-TEST-BOUNDARY";

/// Holds both containers (kept alive for the test) plus the wired app/state.
struct Ctx {
    _pg: ContainerAsync<Postgres>,
    _minio: ContainerAsync<MinIO>,
    pool: PgPool,
    app: axum::Router,
    state: AppState,
}

impl Ctx {
    async fn new(max_upload_bytes: u64) -> Self {
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
        let minio = MinIO::default().start().await.expect("start minio");
        let minio_port = minio.get_host_port_ipv4(9000).await.expect("minio port");
        let endpoint = format!("http://127.0.0.1:{minio_port}");

        let config = test_config(endpoint, max_upload_bytes);
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
        jwt_signing_key: Secret::from("test-signing-key-material-for-file-tests".to_string()),
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
        instrument_id,
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
