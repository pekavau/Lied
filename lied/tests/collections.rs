//! Integration tests for issue #9 (Collections + CollectionItems). Drives the
//! real `/v1` router with bearer auth against a real Postgres (no MinIO — the
//! collection endpoints never touch object storage). Verifies the acceptance
//! criteria: both collection types, item add/reorder/unique-index, the
//! hide-with-references `[removed]` rendering, the permission matrix, and audit.

use std::sync::Arc;

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

fn test_state(pool: PgPool) -> AppState {
    let config = AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from("test-signing-key-material-for-collection-tests".to_string()),
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
    AppState {
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
    let slug = user::slugify(username);
    user::create(pool, id, &slug, username, None, None, username, false, None)
        .await
        .expect("user");
    id
}

async fn create_org(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    organization::create(pool, id, name, &organization::slugify(name), None)
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

fn token(state: &AppState, user_id: Uuid) -> String {
    state.jwt_keyring.mint(user_id, None).expect("mint").0
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

async fn send(
    app: &axum::Router,
    request: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let resp = app.clone().oneshot(request).await.expect("request runs");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

struct Ctx {
    // Keeps the Postgres container alive for the duration of the test.
    _db: TestDb,
    app: axum::Router,
    pool: PgPool,
    org: Uuid,
}

async fn setup(role: membership::Role) -> (Ctx, String) {
    let db = TestDb::create_and_migrate().await;
    let state = test_state(db.pool.clone());
    let org = create_org(&db.pool, "Acme").await;
    let user = create_user(&db.pool, "actor").await;
    add_member(&db.pool, org, user, role).await;
    let tok = token(&state, user);
    let app = lied::routes::build_router(state);
    let pool = db.pool.clone();
    (
        Ctx {
            _db: db,
            app,
            pool,
            org,
        },
        tok,
    )
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_program_and_standing_collections() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let base = format!("/v1/orgs/{}/collections", ctx.org);

    for kind in ["program", "standing"] {
        let (status, body) = send(
            &ctx.app,
            req(
                "POST",
                &base,
                &tok,
                Some(serde_json::json!({ "name": format!("The {kind}"), "type": kind })),
            ),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::CREATED, "create {kind}");
        assert_eq!(body["type"], kind);
    }

    // An invalid type is a 400.
    let (status, _) = send(
        &ctx.app,
        req(
            "POST",
            &base,
            &tok,
            Some(serde_json::json!({ "name": "Bad", "type": "mixtape" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn add_items_reorder_and_unique_index() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let a1 = create_arrangement(&ctx.pool, ctx.org, "alpha").await;
    let a2 = create_arrangement(&ctx.pool, ctx.org, "beta").await;

    let (_s, coll) = send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections", ctx.org),
            &tok,
            Some(serde_json::json!({ "name": "Spring", "type": "program" })),
        ),
    )
    .await;
    let coll_id = coll["id"].as_str().unwrap().to_string();
    let items_url = format!("/v1/orgs/{}/collections/{}/items", ctx.org, coll_id);

    // Add two items at indices 1 and 2.
    let (s1, i1) = send(
        &ctx.app,
        req(
            "POST",
            &items_url,
            &tok,
            Some(serde_json::json!({ "arrangementId": a1, "index": 1 })),
        ),
    )
    .await;
    assert_eq!(s1, axum::http::StatusCode::CREATED);
    let (s2, i2) = send(
        &ctx.app,
        req(
            "POST",
            &items_url,
            &tok,
            Some(serde_json::json!({ "arrangementId": a2, "index": 2 })),
        ),
    )
    .await;
    assert_eq!(s2, axum::http::StatusCode::CREATED);
    let (id1, id2) = (i1["id"].as_str().unwrap(), i2["id"].as_str().unwrap());

    // A duplicate index is rejected with 409.
    let a3 = create_arrangement(&ctx.pool, ctx.org, "gamma").await;
    let (dup, _) = send(
        &ctx.app,
        req(
            "POST",
            &items_url,
            &tok,
            Some(serde_json::json!({ "arrangementId": a3, "index": 1 })),
        ),
    )
    .await;
    assert_eq!(dup, axum::http::StatusCode::CONFLICT);

    // Reorder: swap the two items.
    let (ro, _) = send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections/{}/reorder", ctx.org, coll_id),
            &tok,
            Some(serde_json::json!({ "orderedIds": [id2, id1] })),
        ),
    )
    .await;
    assert_eq!(ro, axum::http::StatusCode::NO_CONTENT);

    // The list now has a2 (beta) first.
    let (_s, list) = send(&ctx.app, req("GET", &items_url, &tok, None)).await;
    let arr = list.as_array().unwrap();
    assert_eq!(arr[0]["arrangementSlug"], "beta");
    assert_eq!(arr[0]["index"], 1);
    assert_eq!(arr[1]["arrangementSlug"], "alpha");
}

#[tokio::test]
async fn soft_deleted_arrangement_renders_item_as_removed() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "doomed").await;

    let (_s, coll) = send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections", ctx.org),
            &tok,
            Some(serde_json::json!({ "name": "Prog", "type": "program" })),
        ),
    )
    .await;
    let coll_id = coll["id"].as_str().unwrap().to_string();
    let items_url = format!("/v1/orgs/{}/collections/{}/items", ctx.org, coll_id);
    send(
        &ctx.app,
        req(
            "POST",
            &items_url,
            &tok,
            Some(serde_json::json!({ "arrangementId": arr, "index": 1 })),
        ),
    )
    .await;

    // Soft-delete the arrangement.
    sqlx::query!(
        r#"UPDATE arrangement SET deleted_at = now() WHERE id = $1"#,
        arr
    )
    .execute(&ctx.pool)
    .await
    .expect("soft-delete");

    // The item is still listed, flagged removed, with the slug shown.
    let (_s, list) = send(&ctx.app, req("GET", &items_url, &tok, None)).await;
    let arr_list = list.as_array().unwrap();
    assert_eq!(arr_list.len(), 1, "item must not vanish");
    assert_eq!(arr_list[0]["arrangementRemoved"], true);
    assert_eq!(arr_list[0]["arrangementSlug"], "doomed");
}

#[tokio::test]
async fn permission_matrix_musician_forbidden_conductor_allowed() {
    // A musician cannot build/edit collections.
    let (ctx, tok) = setup(membership::Role::Musician).await;
    let (status, _) = send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections", ctx.org),
            &tok,
            Some(serde_json::json!({ "name": "Nope", "type": "program" })),
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "musician must be 403"
    );

    // A conductor can (part of the owner/archivist/conductor editor set).
    let (ctx2, tok2) = setup(membership::Role::Conductor).await;
    let (status, _) = send(
        &ctx2.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections", ctx2.org),
            &tok2,
            Some(serde_json::json!({ "name": "Yes", "type": "program" })),
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::CREATED,
        "conductor may build"
    );
}

#[tokio::test]
async fn collection_writes_are_audited() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections", ctx.org),
            &tok,
            Some(serde_json::json!({ "name": "Audited", "type": "standing" })),
        ),
    )
    .await;

    let audited: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "c!" FROM audit_log WHERE action = 'collection.create'"#
    )
    .fetch_one(&ctx.pool)
    .await
    .expect("count");
    assert_eq!(audited, 1, "collection.create must be audited");
}
