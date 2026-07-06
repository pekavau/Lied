//! Integration tests for issue #10 (Part assignments). Drives the real `/v1`
//! router with bearer auth against a real Postgres (no MinIO — the assignment
//! endpoints never touch object storage). Verifies the acceptance criteria:
//! assign/reassign-replaces, permission matrix, cross-org scoping,
//! `notifiedAt`/`acknowledgedAt`, `If-Match`/412, and audit.

use std::sync::Arc;

use lied::config::{AppConfig, Secret};
use lied::domain::membership;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

struct TestDb {
    _pg: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDb {
    async fn create_and_migrate() -> Self {
        let pg = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("start pg");
        let port = pg.get_host_port_ipv4(5432).await.expect("port");
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&format!(
                "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
            ))
            .await
            .expect("connect");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");
        Self { _pg: pg, pool }
    }
}

fn test_state(pool: PgPool) -> lied::state::AppState {
    let config = AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from("test-signing-key-material-for-assignment-tests".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes: 1,
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
    let jwt_keyring = Arc::new(lied::auth::jwt::Keyring::from_single_key(
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
    lied::state::AppState {
        config: Arc::new(config),
        db: pool,
        s3: aws_sdk_s3::Client::from_conf(s3_config),
        metrics: None,
        jwt_keyring,
    }
}

// ── fixtures ─────────────────────────────────────────────────────────────────

async fn create_user(pool: &PgPool, username: &str) -> Uuid {
    let id = Uuid::now_v7();
    let slug = lied::domain::user::slugify(username);
    lied::domain::user::create(pool, id, &slug, username, None, None, username, false, None)
        .await
        .expect("user");
    id
}

async fn create_org(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    lied::domain::organization::create(
        pool,
        id,
        name,
        &lied::domain::organization::slugify(name),
        None,
    )
    .await
    .expect("org");
    id
}

async fn add_member(pool: &PgPool, org_id: Uuid, user_id: Uuid, role: membership::Role) {
    membership::create(
        pool,
        Uuid::now_v7(),
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
    .expect("membership");
}

async fn create_arrangement(pool: &PgPool, org_id: Uuid, slug: &str) -> Uuid {
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

async fn create_voice(pool: &PgPool, arr_id: Uuid, slug: &str) -> Uuid {
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

async fn create_collection(pool: &PgPool, org_id: Uuid, slug: &str) -> Uuid {
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

async fn create_item(pool: &PgPool, coll_id: Uuid, arr_id: Uuid, index: i32) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO collection_item (id, collection_id, arrangement_id, index)
           VALUES ($1, $2, $3, $4)"#,
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

fn token(keyring: &lied::auth::jwt::Keyring, user_id: Uuid) -> String {
    keyring.mint(user_id, None).expect("mint").0
}

fn req(
    method: &str,
    uri: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    match body {
        Some(v) => {
            b = b.header(axum::http::header::CONTENT_TYPE, "application/json");
            b.body(axum::body::Body::from(v.to_string())).unwrap()
        }
        None => b.body(axum::body::Body::empty()).unwrap(),
    }
}

fn req_with_if_match(
    method: &str,
    uri: &str,
    token: &str,
    if_match: Option<&str>,
    body: Option<serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(etag) = if_match {
        b = b.header(axum::http::header::IF_MATCH, etag);
    }
    match body {
        Some(v) => {
            b = b.header(axum::http::header::CONTENT_TYPE, "application/json");
            b.body(axum::body::Body::from(v.to_string())).unwrap()
        }
        None => b.body(axum::body::Body::empty()).unwrap(),
    }
}

async fn send(
    app: &axum::Router,
    request: axum::http::Request<axum::body::Body>,
) -> (
    axum::http::StatusCode,
    serde_json::Value,
    axum::http::HeaderMap,
) {
    let resp = app.clone().oneshot(request).await.expect("request runs");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json, headers)
}

struct Ctx {
    _db: TestDb,
    app: axum::Router,
    pool: PgPool,
    org: Uuid,
    keyring: std::sync::Arc<lied::auth::jwt::Keyring>,
}

impl Ctx {
    fn token_for(&self, user_id: Uuid) -> String {
        token(&self.keyring, user_id)
    }
}

async fn setup(role: membership::Role) -> (Ctx, String, Uuid) {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let org = create_org(&db.pool, "Acme").await;
    let user = create_user(&db.pool, "actor").await;
    add_member(&db.pool, org, user, role).await;
    let keyring = state.jwt_keyring.clone();
    let tok = token(&keyring, user);
    let app = lied::routes::build_router(state);
    let pool = db.pool.clone();
    (
        Ctx {
            _db: db,
            app,
            pool,
            org,
            keyring,
        },
        tok,
        user,
    )
}

/// Build `(collection, item, voice)` fixtures inside `ctx.org`.
async fn seed_item_and_voice(ctx: &Ctx) -> (Uuid, Uuid, Uuid) {
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let voice = create_voice(&ctx.pool, arr, "flute-1").await;
    let coll = create_collection(&ctx.pool, ctx.org, "spring").await;
    let item = create_item(&ctx.pool, coll, arr, 1).await;
    (coll, item, voice)
}

fn assignments_url(org: Uuid, coll: Uuid, item: Uuid) -> String {
    format!("/v1/orgs/{org}/collections/{coll}/items/{item}/assignments")
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn assign_a_user_and_voice_to_an_item() {
    let (ctx, tok, _actor) = setup(membership::Role::Archivist).await;
    let (coll, item, voice) = seed_item_and_voice(&ctx).await;
    let musician = create_user(&ctx.pool, "muso").await;

    let url = assignments_url(ctx.org, coll, item);
    let (status, body, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": musician, "voiceId": voice })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body["userId"], musician.to_string());
    assert_eq!(body["voiceId"], voice.to_string());
    assert!(body["notifiedAt"].is_null());
    assert!(body["acknowledgedAt"].is_null());
}

#[tokio::test]
async fn reassignment_replaces_the_row() {
    let (ctx, tok, _actor) = setup(membership::Role::Archivist).await;
    let (coll, item, voice) = seed_item_and_voice(&ctx).await;
    let alice = create_user(&ctx.pool, "alice").await;
    let bob = create_user(&ctx.pool, "bob").await;

    let url = assignments_url(ctx.org, coll, item);
    let (s1, first, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": alice, "voiceId": voice })),
        ),
    )
    .await;
    assert_eq!(s1, axum::http::StatusCode::CREATED);
    let first_id = first["id"].as_str().unwrap().to_string();

    // Reassign the same (item, voice) to bob.
    let (s2, second, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": bob, "voiceId": voice })),
        ),
    )
    .await;
    assert_eq!(s2, axum::http::StatusCode::OK, "reassignment replaces, 200");
    assert_eq!(second["id"], first_id, "the row keeps its original id");
    assert_eq!(second["userId"], bob.to_string());

    // Still exactly one row for (item, voice).
    let count: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "c!" FROM part_assignment WHERE collection_item_id = $1 AND voice_id = $2"#,
        item,
        voice,
    )
    .fetch_one(&ctx.pool)
    .await
    .expect("count");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn reassigning_to_a_different_user_clears_notified_and_acknowledged() {
    let (ctx, tok, _actor) = setup(membership::Role::Archivist).await;
    let (coll, item, voice) = seed_item_and_voice(&ctx).await;
    let alice = create_user(&ctx.pool, "alice").await;
    let bob = create_user(&ctx.pool, "bob").await;

    let url = assignments_url(ctx.org, coll, item);
    let (_s, first, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": alice, "voiceId": voice })),
        ),
    )
    .await;
    let id = first["id"].as_str().unwrap().to_string();
    let get_url = format!("{url}/{id}");

    // Get its ETag, then mark notified + acknowledged.
    let (_s, _b, headers) = send(&ctx.app, req("GET", &get_url, &tok, None)).await;
    let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();
    let now = chrono::Utc::now();
    let (s, patched, _h) = send(
        &ctx.app,
        req_with_if_match(
            "PATCH",
            &get_url,
            &tok,
            Some(&etag),
            Some(serde_json::json!({ "notifiedAt": now, "acknowledgedAt": now })),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert!(!patched["notifiedAt"].is_null());
    assert!(!patched["acknowledgedAt"].is_null());

    // Reassign to bob — notified/acknowledged reset to null.
    let (_s, reassigned, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": bob, "voiceId": voice })),
        ),
    )
    .await;
    assert_eq!(reassigned["id"], id);
    assert!(
        reassigned["notifiedAt"].is_null(),
        "reassignment must clear notifiedAt"
    );
    assert!(
        reassigned["acknowledgedAt"].is_null(),
        "reassignment must clear acknowledgedAt"
    );
}

#[tokio::test]
async fn voice_must_belong_to_the_items_arrangement() {
    let (ctx, tok, _actor) = setup(membership::Role::Archivist).await;
    let (coll, item, _voice) = seed_item_and_voice(&ctx).await;

    // A voice under a *different* arrangement.
    let other_arr = create_arrangement(&ctx.pool, ctx.org, "nutcracker").await;
    let other_voice = create_voice(&ctx.pool, other_arr, "oboe-1").await;
    let musician = create_user(&ctx.pool, "muso").await;

    let url = assignments_url(ctx.org, coll, item);
    let (status, _body, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": musician, "voiceId": other_voice })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn assigning_a_nonexistent_user_is_rejected() {
    let (ctx, tok, _actor) = setup(membership::Role::Archivist).await;
    let (coll, item, voice) = seed_item_and_voice(&ctx).await;

    let url = assignments_url(ctx.org, coll, item);
    let (status, _body, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": Uuid::now_v7(), "voiceId": voice })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn permission_matrix_musician_forbidden_conductor_allowed() {
    let (ctx, tok, _actor) = setup(membership::Role::Musician).await;
    let (coll, item, voice) = seed_item_and_voice(&ctx).await;
    let musician = create_user(&ctx.pool, "muso").await;
    let url = assignments_url(ctx.org, coll, item);

    let (status, _b, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": musician, "voiceId": voice })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "musician 403");

    let (ctx2, tok2, _actor2) = setup(membership::Role::Conductor).await;
    let (coll2, item2, voice2) = seed_item_and_voice(&ctx2).await;
    let musician2 = create_user(&ctx2.pool, "muso2").await;
    let url2 = assignments_url(ctx2.org, coll2, item2);
    let (status2, _b, _h) = send(
        &ctx2.app,
        req(
            "PUT",
            &url2,
            &tok2,
            Some(serde_json::json!({ "userId": musician2, "voiceId": voice2 })),
        ),
    )
    .await;
    assert_eq!(
        status2,
        axum::http::StatusCode::CREATED,
        "conductor may assign"
    );
}

#[tokio::test]
async fn cross_org_assignment_is_rejected_with_404() {
    let (ctx, _tok, _actor) = setup(membership::Role::Archivist).await;
    let (coll_a, item_a, voice_a) = seed_item_and_voice(&ctx).await;

    // A second org B with its own editor.
    let org_b = create_org(&ctx.pool, "Beta").await;
    let b_editor = create_user(&ctx.pool, "b-editor").await;
    add_member(&ctx.pool, org_b, b_editor, membership::Role::Archivist).await;
    let b_tok = ctx.token_for(b_editor);
    let musician = create_user(&ctx.pool, "muso").await;

    // Org B's editor tries to assign into org A's item via org B's path — 404.
    let url = format!("/v1/orgs/{org_b}/collections/{coll_a}/items/{item_a}/assignments");
    let (status, _b, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &b_tok,
            Some(serde_json::json!({ "userId": musician, "voiceId": voice_a })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_and_get_and_delete_assignment() {
    let (ctx, tok, _actor) = setup(membership::Role::Archivist).await;
    let (coll, item, voice) = seed_item_and_voice(&ctx).await;
    let musician = create_user(&ctx.pool, "muso").await;
    let url = assignments_url(ctx.org, coll, item);

    let (_s, created, _h) = send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": musician, "voiceId": voice })),
        ),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    // List.
    let (status, list, _h) = send(&ctx.app, req("GET", &url, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["id"], id);

    // Get.
    let get_url = format!("{url}/{id}");
    let (status, _b, headers) = send(&ctx.app, req("GET", &get_url, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();

    // Delete requires If-Match.
    let (status, _b, _h) = send(&ctx.app, req("DELETE", &get_url, &tok, None)).await;
    assert_eq!(
        status,
        axum::http::StatusCode::PRECONDITION_FAILED,
        "missing If-Match on delete"
    );
    let (status, _b, _h) = send(
        &ctx.app,
        req_with_if_match("DELETE", &get_url, &tok, Some(&etag), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    // Now gone.
    let (status, _b, _h) = send(&ctx.app, req("GET", &get_url, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn assignment_writes_are_audited() {
    let (ctx, tok, _actor) = setup(membership::Role::Archivist).await;
    let (coll, item, voice) = seed_item_and_voice(&ctx).await;
    let musician = create_user(&ctx.pool, "muso").await;
    let url = assignments_url(ctx.org, coll, item);

    send(
        &ctx.app,
        req(
            "PUT",
            &url,
            &tok,
            Some(serde_json::json!({ "userId": musician, "voiceId": voice })),
        ),
    )
    .await;

    let audited: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "c!" FROM audit_log WHERE action = 'part_assignment.create'"#
    )
    .fetch_one(&ctx.pool)
    .await
    .expect("count");
    assert_eq!(audited, 1, "part_assignment.create must be audited");
}
