//! End-to-end WebDAV tests for issue #8: real app-password Basic-auth requests
//! driven through `build_router` against a real Postgres **and** MinIO. Proves
//! the acceptance criteria — WebDAV↔REST round-trip, per-role scoping/PROPFIND
//! hiding, personal-annotation audit, and private-library isolation.

use std::sync::Arc;

use base64::Engine;
use lied::config::{AppConfig, Secret};
use lied::state::AppState;
use lied::{auth, domain};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

const BUCKET: &str = "lied-test";

struct Ctx {
    _pg: ContainerAsync<Postgres>,
    _minio: ContainerAsync<MinIO>,
    pool: PgPool,
    app: axum::Router,
}

impl Ctx {
    async fn new() -> Self {
        let pg = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("start pg");
        let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
        let pool = PgPoolOptions::new()
            .max_connections(5)
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
        let state = test_state(pool.clone(), endpoint);
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
        }
    }
}

fn test_state(pool: PgPool, s3_endpoint: String) -> AppState {
    let config = AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint,
        s3_bucket: BUCKET.to_string(),
        s3_access_key_id: Secret::from("minioadmin".to_string()),
        s3_secret_access_key: Secret::from("minioadmin".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from("test-signing-key-material-for-webdav-tests".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes: 200 * 1024 * 1024,
        max_request_bytes: 256 * 1024,
        max_files_per_voice: 50,
        max_arrangements_per_org: None,
        max_page_size: 200,
        default_page_size: 50,
        ratelimit_auth_per_min: 100_000,
        ratelimit_anon_per_min: 100_000,
        ratelimit_login_per_min: 100_000,
        metrics_enabled: false,
        docs_enabled: true,
        tracing_pretty: false,
        secure_cookies: false,
    };
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

// ── fixtures (raw SQL + the app-password domain) ─────────────────────────────

/// A seeded user with a WebDAV app password: `(id, username, slug, token)`.
struct SeededUser {
    id: Uuid,
    username: String,
    slug: String,
    token: String,
}

async fn seed_org(pool: &PgPool, slug: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO organization (id, name, slug) VALUES ($1, $2, $3)"#,
        id,
        slug,
        slug,
    )
    .execute(pool)
    .await
    .expect("org");
    id
}

/// Create a user + an app password, and (unless `role` is `None`) a membership.
async fn seed_user(pool: &PgPool, org_id: Uuid, username: &str, role: Option<&str>) -> SeededUser {
    let id = Uuid::now_v7();
    let slug = username.to_string();
    sqlx::query!(
        r#"INSERT INTO "user" (id, slug, username, display_name) VALUES ($1, $2, $3, $4)"#,
        id,
        slug,
        username,
        username,
    )
    .execute(pool)
    .await
    .expect("user");

    if let Some(role) = role {
        sqlx::query!(
            r#"INSERT INTO membership
                 (id, user_id, organization_id, role, instrument_ids,
                  is_principal, principal_instrument_ids)
               VALUES ($1, $2, $3, $4, '{}', false, '{}')"#,
            Uuid::now_v7(),
            id,
            org_id,
            role,
        )
        .execute(pool)
        .await
        .expect("membership");
    }

    let generated = auth::app_password::generate().expect("generate app password");
    domain::app_password::create(
        pool,
        Uuid::now_v7(),
        id,
        "webdav",
        &generated.hash,
        &generated.prefix,
    )
    .await
    .expect("app password");

    SeededUser {
        id,
        username: username.to_string(),
        slug,
        token: generated.plaintext,
    }
}

async fn seed_arr(pool: &PgPool, org_id: Uuid, slug: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO arrangement (id, organization_id, title, slug, status)
           VALUES ($1, $2, $3, $3, 'active')"#,
        id,
        org_id,
        slug,
    )
    .execute(pool)
    .await
    .expect("arrangement");
    id
}

async fn seed_voice(pool: &PgPool, arr_id: Uuid, slug: &str) -> Uuid {
    let instrument_id: Uuid = sqlx::query_scalar!(r#"SELECT id FROM instrument LIMIT 1"#)
        .fetch_one(pool)
        .await
        .expect("instrument");
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO voice (id, arrangement_id, name, slug, instrument_id)
           VALUES ($1, $2, $3, $3, $4)"#,
        id,
        arr_id,
        slug,
        instrument_id,
    )
    .execute(pool)
    .await
    .expect("voice");
    id
}

/// Assign `user` to `voice` (creates a collection + item + part_assignment).
async fn assign(pool: &PgPool, org_id: Uuid, arr_id: Uuid, voice_id: Uuid, user_id: Uuid) {
    let coll = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO collection (id, organization_id, name, slug, type)
           VALUES ($1, $2, 'C', $3, 'program')"#,
        coll,
        org_id,
        format!("c-{}", &coll.to_string()[..8]),
    )
    .execute(pool)
    .await
    .expect("collection");
    let item = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO collection_item (id, collection_id, arrangement_id, index)
           VALUES ($1, $2, $3, 1)"#,
        item,
        coll,
        arr_id,
    )
    .execute(pool)
    .await
    .expect("item");
    sqlx::query!(
        r#"INSERT INTO part_assignment (id, collection_item_id, user_id, voice_id)
           VALUES ($1, $2, $3, $4)"#,
        Uuid::now_v7(),
        item,
        user_id,
        voice_id,
    )
    .execute(pool)
    .await
    .expect("part_assignment");
}

// ── request helpers ──────────────────────────────────────────────────────────

fn basic(u: &SeededUser) -> String {
    let raw = format!("{}:{}", u.username, u.token);
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

fn request(
    method: &str,
    uri: &str,
    auth: &str,
    body: Vec<u8>,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder()
        .method(axum::http::Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, auth);
    if method == "PROPFIND" {
        b = b.header("Depth", "1");
    }
    b.body(axum::body::Body::from(body)).unwrap()
}

async fn send(
    app: &axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.expect("request runs");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn archivist_put_creates_file_row_and_webdav_get_roundtrips() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;

    let uri = "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf";
    let pdf = b"%PDF-1.4 test bytes".to_vec();

    // PUT via WebDAV.
    let (status, _) = send(&ctx.app, request("PUT", uri, &basic(&arch), pdf.clone())).await;
    assert!(status.is_success(), "PUT should succeed, got {status}");

    // A File row now exists at the matching (arr, voice, name, format) — the
    // WebDAV↔REST agreement.
    let row = domain::file::find_by_location(&ctx.pool, arr, Some(voice), "part", "pdf")
        .await
        .expect("query")
        .expect("a File row was created by the WebDAV PUT");
    assert_eq!(row.mime_type, "application/pdf");

    // GET via WebDAV returns the same bytes.
    let (status, body) = send(&ctx.app, request("GET", uri, &basic(&arch), vec![])).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, pdf);
}

#[tokio::test]
async fn musician_propfind_arrangements_root_hides_unassigned() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let alpha = seed_arr(&ctx.pool, org, "alpha").await;
    let _beta = seed_arr(&ctx.pool, org, "beta").await;
    let alpha_voice = seed_voice(&ctx.pool, alpha, "flute-1").await;

    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let musician = seed_user(&ctx.pool, org, "muso", Some("musician")).await;
    assign(&ctx.pool, org, alpha, alpha_voice, musician.id).await;

    let uri = "/orgs/acme/arrangements";

    // Staff see both.
    let (status, body) = send(&ctx.app, request("PROPFIND", uri, &basic(&arch), vec![])).await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("alpha"), "archivist should see alpha");
    assert!(body.contains("beta"), "archivist should see beta");

    // Musician sees only the assigned arrangement.
    let (status, body) = send(
        &ctx.app,
        request("PROPFIND", uri, &basic(&musician), vec![]),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("alpha"), "musician should see assigned alpha");
    assert!(
        !body.contains("beta"),
        "musician must NOT see unassigned beta:\n{body}"
    );
}

#[tokio::test]
async fn private_library_is_isolated_per_user() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let alice = seed_user(&ctx.pool, org, "alice", Some("musician")).await;
    let bob = seed_user(&ctx.pool, org, "bob", Some("musician")).await;

    let uri = "/users/alice/library/etude.pdf";
    let pdf = b"%PDF alice practice".to_vec();

    // Alice writes to her own library and reads it back.
    let (status, _) = send(&ctx.app, request("PUT", uri, &basic(&alice), pdf.clone())).await;
    assert!(
        status.is_success(),
        "alice PUT should succeed, got {status}"
    );
    let (status, body) = send(&ctx.app, request("GET", uri, &basic(&alice), vec![])).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, pdf);

    // Bob cannot read Alice's file or list her library.
    let (status, _) = send(&ctx.app, request("GET", uri, &basic(&bob), vec![])).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (status, _) = send(
        &ctx.app,
        request("PROPFIND", "/users/alice/library", &basic(&bob), vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn musician_annotation_put_is_audited() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let musician = seed_user(&ctx.pool, org, "muso", Some("musician")).await;
    assign(&ctx.pool, org, arr, voice, musician.id).await;

    let uri = format!(
        "/orgs/acme/arrangements/bolero/voices/flute-1/annotations/{}/bowings.pdf",
        musician.slug
    );
    let (status, _) = send(
        &ctx.app,
        request("PUT", &uri, &basic(&musician), b"%PDF bowings".to_vec()),
    )
    .await;
    assert!(
        status.is_success(),
        "annotation PUT should succeed, got {status}"
    );

    let audited: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "c!" FROM audit_log
           WHERE action = 'personal_annotation.create' AND actor_user_id = $1"#,
        musician.id,
    )
    .fetch_one(&ctx.pool)
    .await
    .expect("count");
    assert_eq!(audited, 1, "annotation create must be audited");
}

#[tokio::test]
async fn conductor_cannot_write_files() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let _voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    // Conductor is "staff" for reads but must NOT write files — only
    // owner/archivist may (CLAUDE.md permission matrix).
    let cond = seed_user(&ctx.pool, org, "connie", Some("conductor")).await;

    let uri = "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf";
    let (status, _) = send(
        &ctx.app,
        request("PUT", uri, &basic(&cond), b"%PDF x".to_vec()),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "conductor file PUT must be 403"
    );
}

#[tokio::test]
async fn musician_cannot_write_voice_file() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let muso = seed_user(&ctx.pool, org, "muso", Some("musician")).await;
    assign(&ctx.pool, org, arr, voice, muso.id).await; // assigned → path is visible

    let uri = "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf";
    let (status, _) = send(
        &ctx.app,
        request("PUT", uri, &basic(&muso), b"%PDF x".to_vec()),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "musician file PUT must be 403"
    );
}

#[tokio::test]
async fn full_score_is_hidden_from_a_musician() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let muso = seed_user(&ctx.pool, org, "muso", Some("musician")).await;
    assign(&ctx.pool, org, arr, voice, muso.id).await;

    // Archivist uploads a full score.
    let score_uri = "/orgs/acme/arrangements/bolero/score/full.pdf";
    let (s, _) = send(
        &ctx.app,
        request("PUT", score_uri, &basic(&arch), b"%PDF score".to_vec()),
    )
    .await;
    assert!(s.is_success(), "archivist score PUT should succeed");

    // The `score` dir is absent from the musician's arrangement listing...
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/arrangements/bolero",
            &basic(&muso),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(
        !body.contains("/score"),
        "musician must not see the score dir:\n{body}"
    );
    assert!(
        body.contains("/voices"),
        "musician should see the voices dir"
    );

    // ...and a direct GET of a score file is 404 (hidden, not 403).
    let (status, _) = send(&ctx.app, request("GET", score_uri, &basic(&muso), vec![])).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn non_author_cannot_write_another_users_annotation() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let _alice = seed_user(&ctx.pool, org, "alice", Some("musician")).await;
    let bob = seed_user(&ctx.pool, org, "bob", Some("musician")).await;
    assign(&ctx.pool, org, arr, voice, bob.id).await; // bob may see the voice

    // Bob writes into ALICE's annotations dir → 403 (author-only-writable).
    let uri = "/orgs/acme/arrangements/bolero/voices/flute-1/annotations/alice/x.pdf";
    let (status, _) = send(
        &ctx.app,
        request("PUT", uri, &basic(&bob), b"%PDF x".to_vec()),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn guest_with_assignment_but_no_membership_can_read() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let guest = seed_user(&ctx.pool, org, "guest", None).await; // NO membership
    assign(&ctx.pool, org, arr, voice, guest.id).await;

    // Archivist uploads the part.
    let uri = "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf";
    let pdf = b"%PDF guest part".to_vec();
    let (s, _) = send(&ctx.app, request("PUT", uri, &basic(&arch), pdf.clone())).await;
    assert!(s.is_success());

    // The guest (part assignment only, no membership) can read the part...
    let (status, body) = send(&ctx.app, request("GET", uri, &basic(&guest), vec![])).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, pdf);

    // ...and sees the arrangement at the root.
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/arrangements",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    assert!(String::from_utf8_lossy(&body).contains("bolero"));
}

#[tokio::test]
async fn soft_deleting_the_collection_revokes_guest_access() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let guest = seed_user(&ctx.pool, org, "guest", None).await;
    assign(&ctx.pool, org, arr, voice, guest.id).await;

    // Before deletion: the guest sees the assigned arrangement.
    let (_s, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/arrangements",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert!(String::from_utf8_lossy(&body).contains("bolero"));

    // Soft-delete the collection carrying the guest's assignment.
    sqlx::query!(
        r#"UPDATE collection SET deleted_at = now() WHERE organization_id = $1"#,
        org
    )
    .execute(&ctx.pool)
    .await
    .expect("soft-delete collection");

    // Access reached only through the soft-deleted collection is now gone
    // (hide-with-references): the arrangement disappears from the listing...
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/arrangements",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    assert!(
        !String::from_utf8_lossy(&body).contains("bolero"),
        "guest must lose access through a soft-deleted collection"
    );

    // ...and the voice file is no longer reachable.
    let (status, _) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}
