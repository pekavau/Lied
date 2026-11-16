//! End-to-end WebDAV tests for issue #8: real app-password Basic-auth requests
//! driven through `build_router` against a real Postgres **and** MinIO. Proves
//! the acceptance criteria — WebDAV↔REST round-trip, per-role scoping/PROPFIND
//! hiding, personal-annotation audit, and private-library isolation.

mod common;

use std::sync::Arc;

use base64::Engine;
use lied::config::{AppConfig, Secret};
use lied::state::AppState;
use lied::{auth, domain};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::GenericImage;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

const BUCKET: &str = "lied-test";

struct Ctx {
    _pg: ContainerAsync<Postgres>,
    _store: ContainerAsync<GenericImage>,
    pool: PgPool,
    app: axum::Router,
    s3: aws_sdk_s3::Client,
}

impl Ctx {
    async fn new() -> Self {
        Self::with_max_files(50).await
    }

    async fn with_max_files(max_files_per_voice: u32) -> Self {
        Self::with_limits(max_files_per_voice, 200 * 1024 * 1024).await
    }

    async fn with_limits(max_files_per_voice: u32, max_upload_bytes: u64) -> Self {
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

        // Object storage: the pinned RustFS image (tests/common).
        let (store, endpoint) = common::start_object_store().await;
        let state = test_state(
            pool.clone(),
            endpoint,
            max_files_per_voice,
            max_upload_bytes,
        );
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
            s3: state.s3.clone(),
        }
    }
}

fn test_state(
    pool: PgPool,
    s3_endpoint: String,
    max_files_per_voice: u32,
    max_upload_bytes: u64,
) -> AppState {
    let config = AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint,
        s3_bucket: BUCKET.to_string(),
        s3_access_key_id: Secret::from(common::ACCESS_KEY.to_string()),
        s3_secret_access_key: Secret::from(common::SECRET_KEY.to_string()),
        s3_region: "us-east-1".to_string(),
        s3_version_retention_days: 30,
        jwt_signing_key: Secret::from("test-signing-key-material-for-webdav-tests".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes,
        max_request_bytes: 256 * 1024,
        max_files_per_voice,
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
        r#"WITH v AS (
               INSERT INTO voice (id, arrangement_id, name, slug)
               VALUES ($1, $2, $3, $3) RETURNING id
           )
           INSERT INTO voice_instrument (voice_id, instrument_id, position)
           SELECT id, $4, 1 FROM v"#,
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
        // The whole id: its first hex digits are a UUIDv7's timestamp, the
        // same for two calls in the same millisecond.
        format!("c-{}", coll.simple()),
    )
    .execute(pool)
    .await
    .expect("collection");
    let item = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO collection_item (id, collection_id, arrangement_id, position, label)
           VALUES ($1, $2, $3, 1, '1')"#,
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

async fn seed_collection(pool: &PgPool, org_id: Uuid, slug: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO collection (id, organization_id, name, slug, type)
           VALUES ($1, $2, $3, $3, 'program')"#,
        id,
        org_id,
        slug,
    )
    .execute(pool)
    .await
    .expect("collection");
    id
}

async fn seed_item(pool: &PgPool, coll_id: Uuid, arr_id: Uuid, index: i32) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO collection_item (id, collection_id, arrangement_id, position, label)
           VALUES ($1, $2, $3, $4, ($4::integer)::text)"#,
        id,
        coll_id,
        arr_id,
        index,
    )
    .execute(pool)
    .await
    .expect("item");
    id
}

async fn seed_part_assignment(pool: &PgPool, item_id: Uuid, voice_id: Uuid, user_id: Uuid) {
    sqlx::query!(
        r#"INSERT INTO part_assignment (id, collection_item_id, user_id, voice_id)
           VALUES ($1, $2, $3, $4)"#,
        Uuid::now_v7(),
        item_id,
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

/// `LIED_MAX_FILES_PER_VOICE` over WebDAV (#45): a PUT that would add a file
/// to a full voice is refused with 507 and leaves no row behind; a PUT over
/// an existing file is a replace and still succeeds.
#[tokio::test]
async fn put_into_a_full_voice_is_refused_but_overwriting_succeeds() {
    let ctx = Ctx::with_max_files(1).await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let dir = "/orgs/acme/arrangements/bolero/voices/flute-1";

    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            &format!("{dir}/a.pdf"),
            &basic(&arch),
            b"%PDF a".to_vec(),
        ),
    )
    .await;
    assert!(status.is_success(), "first PUT, got {status}");

    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            &format!("{dir}/b.pdf"),
            &basic(&arch),
            b"%PDF b".to_vec(),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::INSUFFICIENT_STORAGE);
    assert!(
        domain::file::find_by_location(&ctx.pool, arr, Some(voice), "b", "pdf")
            .await
            .unwrap()
            .is_none(),
        "the refused PUT left no row"
    );

    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            &format!("{dir}/a.pdf"),
            &basic(&arch),
            b"%PDF a2".to_vec(),
        ),
    )
    .await;
    assert!(
        status.is_success(),
        "overwrite at the ceiling, got {status}"
    );
    let (_, body) = send(
        &ctx.app,
        request("GET", &format!("{dir}/a.pdf"), &basic(&arch), vec![]),
    )
    .await;
    assert_eq!(body, b"%PDF a2");
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
    // owner/archivist may (docs/design.md permission matrix).
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

// ── collections subtree (issue #10, the phase-1 payoff demo) ────────────────

#[tokio::test]
async fn staff_sees_the_full_collection_tree_in_index_order_including_score() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let alpha = seed_arr(&ctx.pool, org, "alpha").await;
    let alpha_voice = seed_voice(&ctx.pool, alpha, "flute-1").await;
    let beta = seed_arr(&ctx.pool, org, "beta").await;
    let beta_voice = seed_voice(&ctx.pool, beta, "oboe-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;

    // Upload score + voice files for both arrangements via the arrangements tree.
    for (arr_slug, voice_slug) in [("alpha", "flute-1"), ("beta", "oboe-1")] {
        let score_uri = format!("/orgs/acme/arrangements/{arr_slug}/score/full.pdf");
        let (s, _) = send(
            &ctx.app,
            request(
                "PUT",
                &score_uri,
                &basic(&arch),
                format!("%PDF score {arr_slug}").into_bytes(),
            ),
        )
        .await;
        assert!(s.is_success(), "score PUT for {arr_slug}");
        let voice_uri = format!("/orgs/acme/arrangements/{arr_slug}/voices/{voice_slug}/part.pdf");
        let (s, _) = send(
            &ctx.app,
            request(
                "PUT",
                &voice_uri,
                &basic(&arch),
                format!("%PDF part {arr_slug}").into_bytes(),
            ),
        )
        .await;
        assert!(s.is_success(), "voice PUT for {arr_slug}");
    }

    // A program with beta inserted first (index 2) and alpha second (index 1),
    // out of insertion order, to prove the listing sorts by index, not
    // creation order.
    let coll = seed_collection(&ctx.pool, org, "spring").await;
    let _beta_item = seed_item(&ctx.pool, coll, beta, 2).await;
    let _alpha_item = seed_item(&ctx.pool, coll, alpha, 1).await;
    let _ = (alpha_voice, beta_voice);

    // The collections root is visible under the org.
    let (status, body) = send(
        &ctx.app,
        request("PROPFIND", "/orgs/acme", &basic(&arch), vec![]),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    assert!(String::from_utf8_lossy(&body).contains("collections"));

    // The collection appears at /orgs/acme/collections.
    let (status, body) = send(
        &ctx.app,
        request("PROPFIND", "/orgs/acme/collections", &basic(&arch), vec![]),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    assert!(String::from_utf8_lossy(&body).contains("spring"));

    // Both items appear, named `<label padded to 3 digits>-<arrangement-slug>`
    // (#102), in position order.
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/collections/spring",
            &basic(&arch),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("/001-alpha"), "item 1 present:\n{body}");
    assert!(body.contains("/002-beta"), "item 2 present:\n{body}");
    let pos_alpha = body.find("/001-alpha").unwrap();
    let pos_beta = body.find("/002-beta").unwrap();
    assert!(
        pos_alpha < pos_beta,
        "items must be listed in index order:\n{body}"
    );

    // Staff sees both score and voices under an item.
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/collections/spring/001-alpha",
            &basic(&arch),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("/score"), "staff sees score:\n{body}");
    assert!(body.contains("/voices"), "staff sees voices:\n{body}");

    // The score file is readable through the collections tree.
    let (status, body) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/collections/spring/1-alpha/score/full.pdf",
            &basic(&arch),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, b"%PDF score alpha");

    // As is the voice file.
    let (status, body) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/collections/spring/1-alpha/voices/flute-1/part.pdf",
            &basic(&arch),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, b"%PDF part alpha");

    // A standing book with four-digit numbers and a letter label: the names
    // pad to the collection's width, and a plain sort of the names is the
    // order of the pieces (`0001-` < `0001a-` < `0002-`).
    sqlx::query("UPDATE collection SET type = 'standing', number_width = 4 WHERE id = $1")
        .bind(coll)
        .execute(&ctx.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE collection_item SET label = CASE label WHEN '2' THEN '1a' ELSE '1' END \
         WHERE collection_id = $1",
    )
    .bind(coll)
    .execute(&ctx.pool)
    .await
    .unwrap();
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/collections/spring",
            &basic(&arch),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    let mut names: Vec<&str> = ["/0001-alpha", "/0001a-beta"]
        .into_iter()
        .filter(|name| body.contains(name))
        .collect();
    assert_eq!(names.len(), 2, "both padded to four digits:\n{body}");
    names.sort();
    assert_eq!(names, ["/0001-alpha", "/0001a-beta"]);
    // Padded and unpadded paths both resolve to the piece.
    for path in [
        "/orgs/acme/collections/spring/0001a-beta",
        "/orgs/acme/collections/spring/1a-beta",
    ] {
        let (status, _) = send(&ctx.app, request("PROPFIND", path, &basic(&arch), vec![])).await;
        assert_eq!(status.as_u16(), 207, "{path}");
    }
    let (status, _) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/collections/spring/0002-beta",
            &basic(&arch),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 404, "beta is 1a now, not 2");
}

#[tokio::test]
async fn guest_with_assignment_sees_only_their_assigned_voice_no_score() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let flute = seed_voice(&ctx.pool, arr, "flute-1").await;
    let _clarinet = seed_voice(&ctx.pool, arr, "clarinet-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let guest = seed_user(&ctx.pool, org, "guest", None).await; // NO membership

    let (s, _) = send(
        &ctx.app,
        request(
            "PUT",
            "/orgs/acme/arrangements/bolero/score/full.pdf",
            &basic(&arch),
            b"%PDF score".to_vec(),
        ),
    )
    .await;
    assert!(s.is_success());
    let (s, _) = send(
        &ctx.app,
        request(
            "PUT",
            "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf",
            &basic(&arch),
            b"%PDF flute part".to_vec(),
        ),
    )
    .await;
    assert!(s.is_success());
    let (s, _) = send(
        &ctx.app,
        request(
            "PUT",
            "/orgs/acme/arrangements/bolero/voices/clarinet-1/part.pdf",
            &basic(&arch),
            b"%PDF clarinet part".to_vec(),
        ),
    )
    .await;
    assert!(s.is_success());

    let coll = seed_collection(&ctx.pool, org, "spring").await;
    let item = seed_item(&ctx.pool, coll, arr, 1).await;
    seed_part_assignment(&ctx.pool, item, flute, guest.id).await;

    // The guest sees the collection and the item (in order).
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/collections/spring",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    assert!(String::from_utf8_lossy(&body).contains("1-bolero"));

    // No score dir in the item listing.
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/collections/spring/1-bolero",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(
        !body.contains("/score"),
        "guest must not see score:\n{body}"
    );
    assert!(body.contains("/voices"), "guest sees voices dir:\n{body}");

    // Only the assigned voice appears in the voices listing.
    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/collections/spring/1-bolero/voices",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("flute-1"), "assigned voice visible:\n{body}");
    assert!(
        !body.contains("clarinet-1"),
        "unassigned voice hidden:\n{body}"
    );

    // Direct GET of the score is 404 (hidden, not 403).
    let (status, _) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/collections/spring/1-bolero/score/full.pdf",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // The assigned voice's part is readable...
    let (status, body) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/collections/spring/1-bolero/voices/flute-1/part.pdf",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, b"%PDF flute part");

    // ...but the unassigned voice's part is not.
    let (status, _) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/collections/spring/1-bolero/voices/clarinet-1/part.pdf",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn musician_member_sees_only_assigned_voice_no_score_in_collections_tree() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let flute = seed_voice(&ctx.pool, arr, "flute-1").await;
    let _clarinet = seed_voice(&ctx.pool, arr, "clarinet-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let muso = seed_user(&ctx.pool, org, "muso", Some("musician")).await;

    send(
        &ctx.app,
        request(
            "PUT",
            "/orgs/acme/arrangements/bolero/score/full.pdf",
            &basic(&arch),
            b"%PDF score".to_vec(),
        ),
    )
    .await;
    send(
        &ctx.app,
        request(
            "PUT",
            "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf",
            &basic(&arch),
            b"%PDF flute part".to_vec(),
        ),
    )
    .await;

    let coll = seed_collection(&ctx.pool, org, "spring").await;
    let item = seed_item(&ctx.pool, coll, arr, 1).await;
    seed_part_assignment(&ctx.pool, item, flute, muso.id).await;

    let (status, body) = send(
        &ctx.app,
        request(
            "PROPFIND",
            "/orgs/acme/collections/spring/1-bolero",
            &basic(&muso),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(
        !body.contains("/score"),
        "musician must not see full score:\n{body}"
    );

    let (status, _) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/collections/spring/1-bolero/voices/flute-1/part.pdf",
            &basic(&muso),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn collections_subtree_is_read_only() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;

    send(
        &ctx.app,
        request(
            "PUT",
            "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf",
            &basic(&arch),
            b"%PDF x".to_vec(),
        ),
    )
    .await;

    let coll = seed_collection(&ctx.pool, org, "spring").await;
    let item = seed_item(&ctx.pool, coll, arr, 1).await;
    seed_part_assignment(&ctx.pool, item, voice, arch.id).await;

    // Archivist (staff, write-capable on the arrangements tree) still cannot
    // write through the read-only collections view.
    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            "/orgs/acme/collections/spring/1-bolero/voices/flute-1/part.pdf",
            &basic(&arch),
            b"%PDF replacement".to_vec(),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "PUT rejected");

    let (status, _) = send(
        &ctx.app,
        request(
            "DELETE",
            "/orgs/acme/collections/spring/1-bolero/voices/flute-1/part.pdf",
            &basic(&arch),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "DELETE rejected");

    let (status, _) = send(
        &ctx.app,
        request(
            "MKCOL",
            "/orgs/acme/collections/spring/1-bolero/voices/new-voice",
            &basic(&arch),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "MKCOL rejected");
}

#[tokio::test]
async fn assignment_visibility_is_scoped_to_the_specific_item_not_the_whole_arrangement() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let guest = seed_user(&ctx.pool, org, "guest", None).await;

    send(
        &ctx.app,
        request(
            "PUT",
            "/orgs/acme/arrangements/bolero/voices/flute-1/part.pdf",
            &basic(&arch),
            b"%PDF x".to_vec(),
        ),
    )
    .await;

    // Two different programs both feature the same arrangement/voice. The
    // guest is assigned on item A (in collection "spring") but NOT on item B
    // (in collection "summer") — visibility must be scoped to item A only.
    let coll_a = seed_collection(&ctx.pool, org, "spring").await;
    let item_a = seed_item(&ctx.pool, coll_a, arr, 1).await;
    seed_part_assignment(&ctx.pool, item_a, voice, guest.id).await;

    let coll_b = seed_collection(&ctx.pool, org, "summer").await;
    let _item_b = seed_item(&ctx.pool, coll_b, arr, 1).await; // no assignment here

    // Readable through item A.
    let (status, _) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/collections/spring/1-bolero/voices/flute-1/part.pdf",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // NOT reachable through item B, despite being the same arrangement/voice.
    let (status, _) = send(
        &ctx.app,
        request(
            "GET",
            "/orgs/acme/collections/summer/1-bolero/voices/flute-1/part.pdf",
            &basic(&guest),
            vec![],
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "assignment on item A must not leak visibility to item B"
    );

    // And "summer" itself is entirely absent from the guest's collections listing.
    let (status, body) = send(
        &ctx.app,
        request("PROPFIND", "/orgs/acme/collections", &basic(&guest), vec![]),
    )
    .await;
    assert_eq!(status.as_u16(), 207);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("spring"), "spring visible:\n{body}");
    assert!(!body.contains("summer"), "summer hidden:\n{body}");
}

// ── streaming, rollback, MOVE / COPY / remove_dir (#26, #109) ────────────────

const PART: usize = lied::storage::UPLOAD_PART_SIZE;

/// A deterministic, non-repeating-per-part byte pattern.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// A request whose body arrives in `chunk`-sized frames, as a client's does.
fn streamed(
    method: &str,
    uri: &str,
    auth: &str,
    data: &[u8],
    chunk: usize,
) -> axum::http::Request<axum::body::Body> {
    let frames: Vec<Result<bytes::Bytes, std::io::Error>> = data
        .chunks(chunk)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();
    axum::http::Request::builder()
        .method(axum::http::Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, auth)
        .body(axum::body::Body::from_stream(futures_util::stream::iter(
            frames,
        )))
        .unwrap()
}

fn request_with(
    method: &str,
    uri: &str,
    auth: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder()
        .method(axum::http::Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, auth);
    for (name, value) in headers {
        b = b.header(*name, *value);
    }
    b.body(axum::body::Body::from(body)).unwrap()
}

async fn open_multipart_uploads(ctx: &Ctx) -> usize {
    ctx.s3
        .list_multipart_uploads()
        .bucket(BUCKET)
        .send()
        .await
        .expect("list multipart uploads")
        .uploads()
        .len()
}

async fn file_rows_named(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM file WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A file larger than two parts goes through WebDAV in parts: never more
/// than one part in memory on the way up, uploaded as a 3-part multipart
/// object, and read back (whole and ranged) from the storage stream.
#[tokio::test]
async fn large_files_stream_through_webdav_in_parts() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let uri = "/orgs/acme/arrangements/bolero/voices/flute-1/big.pdf";
    let data = pattern(2 * PART + 3 * 1024 * 1024);

    let (status, _) = send(
        &ctx.app,
        streamed("PUT", uri, &basic(&arch), &data, 64 * 1024),
    )
    .await;
    assert!(status.is_success(), "PUT, got {status}");
    assert!(
        lied::storage::peak_upload_buffer() <= PART,
        "an upload held {} bytes, more than one part ({PART})",
        lied::storage::peak_upload_buffer()
    );
    let key = domain::file::derived_key("acme", "bolero", Some("flute-1"), "big", "pdf");
    let head = ctx
        .s3
        .head_object()
        .bucket(BUCKET)
        .key(&key)
        .send()
        .await
        .expect("the object exists");
    assert!(
        head.e_tag().unwrap_or_default().ends_with("-3\""),
        "uploaded as three parts: {:?}",
        head.e_tag()
    );

    let (status, body) = send(&ctx.app, request("GET", uri, &basic(&arch), vec![])).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body == data, "the whole file reads back byte for byte");

    // A ranged read seeks: the stream is re-requested from the offset.
    let (from, to) = (PART + 5, PART + 104);
    let range = format!("bytes={from}-{to}");
    let (status, body) = send(
        &ctx.app,
        request_with("GET", uri, &basic(&arch), vec![], &[("Range", &range)]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &data[from..=to]);
    let (status, body) = send(
        &ctx.app,
        request_with("GET", uri, &basic(&arch), vec![], &[("Range", "bytes=-10")]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &data[data.len() - 10..]);
}

/// Past `LIED_MAX_UPLOAD_BYTES` while streaming: 413, and nothing left
/// behind — no row, no object, no open multipart upload.
#[tokio::test]
async fn a_too_large_put_is_refused_and_leaves_nothing() {
    let ctx = Ctx::with_limits(50, (PART + 1024 * 1024) as u64).await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let uri = "/orgs/acme/arrangements/bolero/voices/flute-1/huge.pdf";

    let (status, _) = send(
        &ctx.app,
        streamed("PUT", uri, &basic(&arch), &pattern(2 * PART), 64 * 1024),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(file_rows_named(&ctx.pool, "huge").await, 0, "no row");
    let key = domain::file::derived_key("acme", "bolero", Some("flute-1"), "huge", "pdf");
    assert!(
        ctx.s3
            .head_object()
            .bucket(BUCKET)
            .key(&key)
            .send()
            .await
            .is_err(),
        "no object"
    );
    assert_eq!(
        open_multipart_uploads(&ctx).await,
        0,
        "the multipart upload was aborted"
    );
}

/// #109: when the bytes never reach storage, the row write is taken back the
/// way the `/v1` path takes it back. A new file leaves no row at all (before,
/// a soft-deleted one stayed, restorable into a file with no content); a
/// replace leaves the old file live.
#[tokio::test]
async fn a_failed_store_leaves_no_row_and_a_failed_replace_keeps_the_old_file() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let dir = "/orgs/acme/arrangements/bolero/voices/flute-1";
    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            &format!("{dir}/a.pdf"),
            &basic(&arch),
            b"%PDF a".to_vec(),
        ),
    )
    .await;
    assert!(status.is_success(), "{status}");
    let old = domain::file::find_by_location(&ctx.pool, arr, Some(voice), "a", "pdf")
        .await
        .unwrap()
        .expect("a.pdf");

    // Storage goes away: empty the bucket and remove it.
    let key = domain::file::derived_key("acme", "bolero", Some("flute-1"), "a", "pdf");
    ctx.s3
        .delete_object()
        .bucket(BUCKET)
        .key(&key)
        .send()
        .await
        .unwrap();
    ctx.s3.delete_bucket().bucket(BUCKET).send().await.unwrap();

    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            &format!("{dir}/b.pdf"),
            &basic(&arch),
            b"%PDF b".to_vec(),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        file_rows_named(&ctx.pool, "b").await,
        0,
        "no row for b.pdf, neither live nor soft-deleted"
    );

    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            &format!("{dir}/a.pdf"),
            &basic(&arch),
            b"%PDF a2".to_vec(),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let live = domain::file::find_by_location(&ctx.pool, arr, Some(voice), "a", "pdf")
        .await
        .unwrap()
        .expect("a.pdf is still live");
    assert_eq!(live.id, old.id, "the old file is the live one again");
    assert_eq!(
        file_rows_named(&ctx.pool, "a").await,
        1,
        "the new row is gone"
    );
}

/// #45 over WebDAV, early: a new file in a full voice is refused when the PUT
/// opens, before the client's body is read, so no bytes travel and no
/// multipart upload starts.
#[tokio::test]
async fn a_put_into_a_full_voice_is_refused_before_its_body() {
    let ctx = Ctx::with_max_files(1).await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let dir = "/orgs/acme/arrangements/bolero/voices/flute-1";
    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            &format!("{dir}/a.pdf"),
            &basic(&arch),
            b"%PDF a".to_vec(),
        ),
    )
    .await;
    assert!(status.is_success(), "{status}");

    let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = polled.clone();
    let body = futures_util::stream::once(async move {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok::<_, std::io::Error>(bytes::Bytes::from(pattern(2 * PART)))
    });
    let req = axum::http::Request::builder()
        .method("PUT")
        .uri(format!("{dir}/b.pdf"))
        .header(axum::http::header::AUTHORIZATION, basic(&arch))
        .body(axum::body::Body::from_stream(body))
        .unwrap();
    let (status, _) = send(&ctx.app, req).await;
    assert_eq!(status, axum::http::StatusCode::INSUFFICIENT_STORAGE);
    assert!(
        !polled.load(std::sync::atomic::Ordering::SeqCst),
        "the body was never read"
    );
    assert_eq!(open_multipart_uploads(&ctx).await, 0);
}

/// The private library is plain storage: files and directories move, files
/// copy, and an empty directory can be removed. A non-empty one survives a
/// `Depth: 0` DELETE; the default DELETE removes it with its contents.
#[tokio::test]
async fn library_files_and_directories_move_copy_and_are_removed() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let alice = seed_user(&ctx.pool, org, "alice", Some("musician")).await;
    let auth = basic(&alice);
    let lib = "/users/alice/library";
    let pdf = b"%PDF etude".to_vec();

    let (status, _) = send(
        &ctx.app,
        request("MKCOL", &format!("{lib}/set"), &auth, vec![]),
    )
    .await;
    assert!(status.is_success(), "MKCOL {status}");
    let (status, _) = send(
        &ctx.app,
        request("PUT", &format!("{lib}/set/a.pdf"), &auth, pdf.clone()),
    )
    .await;
    assert!(status.is_success(), "PUT {status}");

    // MOVE a file (rename).
    let dest = format!("{lib}/set/b.pdf");
    let (status, _) = send(
        &ctx.app,
        request_with(
            "MOVE",
            &format!("{lib}/set/a.pdf"),
            &auth,
            vec![],
            &[("Destination", &dest)],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let (status, _) = send(
        &ctx.app,
        request("GET", &format!("{lib}/set/a.pdf"), &auth, vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (_, body) = send(&ctx.app, request("GET", &dest, &auth, vec![])).await;
    assert_eq!(body, pdf);

    // MOVE a directory.
    let dest = format!("{lib}/set2");
    let (status, _) = send(
        &ctx.app,
        request_with(
            "MOVE",
            &format!("{lib}/set"),
            &auth,
            vec![],
            &[("Destination", &dest)],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let (status, body) = send(
        &ctx.app,
        request("GET", &format!("{lib}/set2/b.pdf"), &auth, vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, pdf);
    let (status, _) = send(
        &ctx.app,
        request("PROPFIND", &format!("{lib}/set"), &auth, vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // COPY a file; Overwrite: F onto an existing one is refused.
    let dest = format!("{lib}/set2/c.pdf");
    let (status, _) = send(
        &ctx.app,
        request_with(
            "COPY",
            &format!("{lib}/set2/b.pdf"),
            &auth,
            vec![],
            &[("Destination", &dest)],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let (_, body) = send(&ctx.app, request("GET", &dest, &auth, vec![])).await;
    assert_eq!(body, pdf);
    let (status, _) = send(
        &ctx.app,
        request_with(
            "MOVE",
            &format!("{lib}/set2/b.pdf"),
            &auth,
            vec![],
            &[("Destination", &dest), ("Overwrite", "F")],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::PRECONDITION_FAILED);

    // A non-empty directory is not removed by itself...
    let (status, _) = send(
        &ctx.app,
        request_with(
            "DELETE",
            &format!("{lib}/set2"),
            &auth,
            vec![],
            &[("Depth", "0")],
        ),
    )
    .await;
    assert!(
        !status.is_success(),
        "Depth: 0 on a non-empty directory, got {status}"
    );
    let (status, _) = send(
        &ctx.app,
        request("GET", &format!("{lib}/set2/b.pdf"), &auth, vec![]),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "its contents are intact"
    );

    // ...but the default DELETE takes it with its contents,
    let (status, _) = send(
        &ctx.app,
        request("DELETE", &format!("{lib}/set2"), &auth, vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (status, _) = send(
        &ctx.app,
        request("PROPFIND", &format!("{lib}/set2"), &auth, vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // and an empty directory goes too.
    let (status, _) = send(
        &ctx.app,
        request("MKCOL", &format!("{lib}/empty"), &auth, vec![]),
    )
    .await;
    assert!(status.is_success(), "MKCOL {status}");
    let (status, _) = send(
        &ctx.app,
        request("DELETE", &format!("{lib}/empty"), &auth, vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (status, _) = send(
        &ctx.app,
        request("PROPFIND", &format!("{lib}/empty"), &auth, vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

/// In the org tree a file is a `File` row and a directory an entity, so
/// MOVE, COPY and directory DELETE are refused with 403 — before dav-server
/// runs, because it would delete the destination (or the directory's files)
/// first. A personal annotation may still be renamed within its directory.
#[tokio::test]
async fn org_tree_move_copy_and_directory_delete_are_refused() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let arr = seed_arr(&ctx.pool, org, "bolero").await;
    let voice = seed_voice(&ctx.pool, arr, "flute-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let auth = basic(&arch);
    let dir = "/orgs/acme/arrangements/bolero/voices/flute-1";
    for name in ["a", "b"] {
        let (status, _) = send(
            &ctx.app,
            request("PUT", &format!("{dir}/{name}.pdf"), &auth, b"%PDF".to_vec()),
        )
        .await;
        assert!(status.is_success(), "{status}");
    }
    let live = |name: &'static str| {
        let pool = ctx.pool.clone();
        async move {
            domain::file::find_by_location(&pool, arr, Some(voice), name, "pdf")
                .await
                .unwrap()
                .is_some()
        }
    };

    // MOVE onto an existing file (Overwrite defaults to T).
    let dest = format!("{dir}/b.pdf");
    let (status, _) = send(
        &ctx.app,
        request_with(
            "MOVE",
            &format!("{dir}/a.pdf"),
            &auth,
            vec![],
            &[("Destination", &dest)],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert!(
        live("a").await && live("b").await,
        "both files are untouched"
    );

    let (status, _) = send(
        &ctx.app,
        request_with(
            "COPY",
            &format!("{dir}/a.pdf"),
            &auth,
            vec![],
            &[("Destination", &format!("{dir}/c.pdf"))],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

    // From the library into the org tree is refused too.
    let (status, _) = send(
        &ctx.app,
        request(
            "PUT",
            "/users/arch/library/x.pdf",
            &auth,
            b"%PDF x".to_vec(),
        ),
    )
    .await;
    assert!(status.is_success(), "{status}");
    let (status, _) = send(
        &ctx.app,
        request_with(
            "MOVE",
            "/users/arch/library/x.pdf",
            &auth,
            vec![],
            &[("Destination", &format!("{dir}/x.pdf"))],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

    // DELETE of a voice directory would delete its files first.
    let (status, _) = send(&ctx.app, request("DELETE", dir, &auth, vec![])).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert!(
        live("a").await && live("b").await,
        "the voice's files survive"
    );

    // A personal annotation renames within its own directory, audited.
    let notes = format!("{dir}/annotations/arch");
    let (status, _) = send(
        &ctx.app,
        request("PUT", &format!("{notes}/n.pdf"), &auth, b"%PDF n".to_vec()),
    )
    .await;
    assert!(status.is_success(), "{status}");
    let (status, _) = send(
        &ctx.app,
        request_with(
            "MOVE",
            &format!("{notes}/n.pdf"),
            &auth,
            vec![],
            &[("Destination", &format!("{notes}/m.pdf"))],
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let (status, body) = send(
        &ctx.app,
        request("GET", &format!("{notes}/m.pdf"), &auth, vec![]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, b"%PDF n");
    let renamed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'personal_annotation.rename'",
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(renamed, 1);
}

// ── #119: the org's archive setting, over WebDAV as over /v1 ────────────────

async fn propfind_body(ctx: &Ctx, uri: &str, user: &SeededUser) -> (u16, String) {
    let (status, body) = send(&ctx.app, request("PROPFIND", uri, &basic(user), vec![])).await;
    (status.as_u16(), String::from_utf8_lossy(&body).into_owned())
}

async fn get_status(ctx: &Ctx, uri: &str, user: &SeededUser) -> u16 {
    send(&ctx.app, request("GET", uri, &basic(user), vec![]))
        .await
        .0
        .as_u16()
}

/// With the archive closed (the default) a musician sees their assigned
/// parts and nothing else; once the owner opens it they see every
/// arrangement, voice, score and collection, read-only. A guest sees their
/// part either way. The rule is `domain::access::read_scope`, which `/v1`
/// uses too.
#[tokio::test]
async fn musicians_see_the_whole_tree_only_when_the_org_opens_its_archive() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let alpha = seed_arr(&ctx.pool, org, "alpha").await;
    let beta = seed_arr(&ctx.pool, org, "beta").await;
    let alpha_flute = seed_voice(&ctx.pool, alpha, "flute-1").await;
    let _alpha_oboe = seed_voice(&ctx.pool, alpha, "oboe-1").await;
    let _beta_horn = seed_voice(&ctx.pool, beta, "horn-1").await;
    let arch = seed_user(&ctx.pool, org, "arch", Some("archivist")).await;
    let muso = seed_user(&ctx.pool, org, "muso", Some("musician")).await;
    let guest = seed_user(&ctx.pool, org, "guest", None).await;
    assign(&ctx.pool, org, alpha, alpha_flute, muso.id).await;
    assign(&ctx.pool, org, alpha, alpha_flute, guest.id).await;
    // A collection holding only beta, nobody assigned in it.
    let summer = seed_collection(&ctx.pool, org, "summer").await;
    seed_item(&ctx.pool, summer, beta, 1).await;

    let files = [
        "/orgs/acme/arrangements/alpha/voices/flute-1/part.pdf",
        "/orgs/acme/arrangements/alpha/voices/oboe-1/part.pdf",
        "/orgs/acme/arrangements/alpha/score/full.pdf",
        "/orgs/acme/arrangements/beta/voices/horn-1/part.pdf",
        "/orgs/acme/arrangements/beta/score/full.pdf",
    ];
    for uri in files {
        let (s, _) = send(
            &ctx.app,
            request("PUT", uri, &basic(&arch), b"%PDF part".to_vec()),
        )
        .await;
        assert!(s.is_success(), "archivist PUT {uri}: {s}");
    }

    // ── archive closed ──
    let (status, root) = propfind_body(&ctx, "/orgs/acme/arrangements", &muso).await;
    assert_eq!(status, 207);
    assert!(root.contains("alpha") && !root.contains("beta"), "{root}");
    let (_, alpha_dir) = propfind_body(&ctx, "/orgs/acme/arrangements/alpha", &muso).await;
    assert!(!alpha_dir.contains("/score"), "{alpha_dir}");
    let (_, collections) = propfind_body(&ctx, "/orgs/acme/collections", &muso).await;
    assert!(!collections.contains("summer"), "{collections}");
    let statuses: Vec<u16> = get_statuses(&ctx, &files, &muso).await;
    assert_eq!(statuses, [200, 404, 404, 404, 404]);

    // ── the owner opens the archive ──
    lied::domain::organization::set_musicians_read_archive(&ctx.pool, org, None, true)
        .await
        .unwrap()
        .expect("org");

    let (_, root) = propfind_body(&ctx, "/orgs/acme/arrangements", &muso).await;
    assert!(root.contains("alpha") && root.contains("beta"), "{root}");
    let (_, alpha_dir) = propfind_body(&ctx, "/orgs/acme/arrangements/alpha", &muso).await;
    assert!(alpha_dir.contains("/score"), "{alpha_dir}");
    let (_, voices) = propfind_body(&ctx, "/orgs/acme/arrangements/alpha/voices", &muso).await;
    assert!(voices.contains("oboe-1"), "{voices}");
    let (_, collections) = propfind_body(&ctx, "/orgs/acme/collections", &muso).await;
    assert!(collections.contains("summer"), "{collections}");
    let statuses: Vec<u16> = get_statuses(&ctx, &files, &muso).await;
    assert_eq!(statuses, [200, 200, 200, 200, 200]);

    // Read-only: no file writes, no deletes.
    for uri in [files[1], files[4]] {
        let (s, _) = send(
            &ctx.app,
            request("PUT", uri, &basic(&muso), b"%PDF mine".to_vec()),
        )
        .await;
        assert_eq!(s.as_u16(), 403, "PUT {uri}");
        let (s, _) = send(&ctx.app, request("DELETE", uri, &basic(&muso), vec![])).await;
        assert_eq!(s.as_u16(), 403, "DELETE {uri}");
    }
    assert_eq!(get_status(&ctx, files[1], &arch).await, 200, "file intact");

    // A guest is never widened.
    let (_, root) = propfind_body(&ctx, "/orgs/acme/arrangements", &guest).await;
    assert!(root.contains("alpha") && !root.contains("beta"), "{root}");
    let statuses: Vec<u16> = get_statuses(&ctx, &files, &guest).await;
    assert_eq!(statuses, [200, 404, 404, 404, 404]);

    // Staff are unchanged.
    let statuses: Vec<u16> = get_statuses(&ctx, &files, &arch).await;
    assert_eq!(statuses, [200, 200, 200, 200, 200]);
}

/// GET each of `uris` as `user`, in order.
async fn get_statuses(ctx: &Ctx, uris: &[&str], user: &SeededUser) -> Vec<u16> {
    let mut statuses = Vec::new();
    for uri in uris {
        statuses.push(get_status(ctx, uri, user).await);
    }
    statuses
}

// ── #124: personal annotations follow the part; system admins ───────────────

async fn put_status(ctx: &Ctx, uri: &str, user: &SeededUser) -> u16 {
    send(
        &ctx.app,
        request("PUT", uri, &basic(user), b"%PDF marks".to_vec()),
    )
    .await
    .0
    .as_u16()
}

/// Personal annotations are read and written only under a voice the caller
/// plays, guests included (a substitute marks up their part and sees the
/// regular player's markings). Opening the archive does not widen this: a
/// musician reads every voice's files, but not the annotations of a part
/// they don't play, and writes none there. Staff read all and annotate any
/// voice.
#[tokio::test]
async fn personal_annotations_follow_the_part_not_the_archive() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let alpha = seed_arr(&ctx.pool, org, "alpha").await;
    let flute = seed_voice(&ctx.pool, alpha, "flute-1").await;
    let _oboe = seed_voice(&ctx.pool, alpha, "oboe-1").await;
    let cond = seed_user(&ctx.pool, org, "cond", Some("conductor")).await;
    let regular = seed_user(&ctx.pool, org, "regular", Some("musician")).await;
    let muso = seed_user(&ctx.pool, org, "muso", Some("musician")).await;
    let sub = seed_user(&ctx.pool, org, "sub", None).await;
    assign(&ctx.pool, org, alpha, flute, regular.id).await;
    assign(&ctx.pool, org, alpha, flute, sub.id).await;
    lied::domain::organization::set_musicians_read_archive(&ctx.pool, org, None, true)
        .await
        .unwrap()
        .expect("org");

    let flute_dir = "/orgs/acme/arrangements/alpha/voices/flute-1";
    let ann = |voice: &str, user: &str| {
        format!("/orgs/acme/arrangements/alpha/voices/{voice}/annotations/{user}/marks.pdf")
    };
    let regular_marks = ann("flute-1", "regular");

    // The regular player and the substitute (a guest) annotate their part...
    assert!(put_status(&ctx, &regular_marks, &regular).await < 300);
    assert!(put_status(&ctx, &ann("flute-1", "sub"), &sub).await < 300);
    // ...and the substitute reads the regular's markings.
    assert_eq!(get_status(&ctx, &regular_marks, &sub).await, 200);
    // A guest has no other voice at all: the voice is absent, so the PUT has
    // no parent collection (dav-server's 409).
    assert_eq!(put_status(&ctx, &ann("oboe-1", "sub"), &sub).await, 409);

    // An open-archive musician reads the voice, but not its annotations...
    let (status, listing) = propfind_body(&ctx, flute_dir, &muso).await;
    assert_eq!(status, 207);
    assert!(!listing.contains("annotations"), "{listing}");
    assert_eq!(get_status(&ctx, &regular_marks, &muso).await, 404);
    let (status, _) = propfind_body(&ctx, &format!("{flute_dir}/annotations"), &muso).await;
    assert_eq!(status, 404);
    // ...and writes none there.
    assert_eq!(put_status(&ctx, &ann("flute-1", "muso"), &muso).await, 403);
    let (status, _) = send(
        &ctx.app,
        request(
            "MKCOL",
            &format!("{flute_dir}/annotations/muso"),
            &basic(&muso),
            vec![],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 403);

    // The player still sees the annotations folder on their own part.
    let (_, listing) = propfind_body(&ctx, flute_dir, &regular).await;
    assert!(listing.contains("annotations"), "{listing}");

    // Staff read everyone's and annotate any voice.
    assert_eq!(get_status(&ctx, &regular_marks, &cond).await, 200);
    assert!(put_status(&ctx, &ann("oboe-1", "cond"), &cond).await < 300);
}

/// A system admin is owner-equivalent in every org over WebDAV as over
/// `/v1`, member or not: the org is in their `/orgs` root, and they read and
/// write its archive. A non-member who is not an admin sees none of it.
#[tokio::test]
async fn a_system_admin_acts_as_owner_over_webdav() {
    let ctx = Ctx::new().await;
    let org = seed_org(&ctx.pool, "acme").await;
    let alpha = seed_arr(&ctx.pool, org, "alpha").await;
    seed_voice(&ctx.pool, alpha, "flute-1").await;
    let admin = seed_user(&ctx.pool, org, "admin", None).await;
    sqlx::query(r#"UPDATE "user" SET is_system_admin = true WHERE id = $1"#)
        .bind(admin.id)
        .execute(&ctx.pool)
        .await
        .unwrap();
    let outsider = seed_user(&ctx.pool, org, "outsider", None).await;

    let (status, orgs) = propfind_body(&ctx, "/orgs", &admin).await;
    assert_eq!(status, 207);
    assert!(orgs.contains("acme"), "{orgs}");
    let (_, orgs) = propfind_body(&ctx, "/orgs", &outsider).await;
    assert!(!orgs.contains("acme"), "{orgs}");

    let score = "/orgs/acme/arrangements/alpha/score/full.pdf";
    let part = "/orgs/acme/arrangements/alpha/voices/flute-1/part.pdf";
    assert!(put_status(&ctx, score, &admin).await < 300);
    assert!(put_status(&ctx, part, &admin).await < 300);
    assert_eq!(get_statuses(&ctx, &[score, part], &admin).await, [200, 200]);
    assert_eq!(
        get_statuses(&ctx, &[score, part], &outsider).await,
        [404, 404]
    );
}
