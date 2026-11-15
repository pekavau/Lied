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
    keyring: std::sync::Arc<lied::auth::jwt::Keyring>,
}

impl Ctx {
    /// Mint a bearer token for `user_id` against the app's own keyring.
    fn token_for(&self, user_id: Uuid) -> String {
        token(&self.keyring, user_id)
    }
}

async fn setup(role: membership::Role) -> (Ctx, String) {
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
            Some(serde_json::json!({ "arrangementId": a1, "position": 1 })),
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
            Some(serde_json::json!({ "arrangementId": a2, "position": 2 })),
        ),
    )
    .await;
    assert_eq!(s2, axum::http::StatusCode::CREATED);
    let (id1, id2) = (i1["id"].as_str().unwrap(), i2["id"].as_str().unwrap());

    // A position past the end (n + 1 = 3) is refused; a program has no gaps.
    let a3 = create_arrangement(&ctx.pool, ctx.org, "gamma").await;
    let (past, _) = send(
        &ctx.app,
        req(
            "POST",
            &items_url,
            &tok,
            Some(serde_json::json!({ "arrangementId": a3, "position": 4 })),
        ),
    )
    .await;
    assert_eq!(past, axum::http::StatusCode::BAD_REQUEST);
    // The same arrangement twice is a 409.
    let (dup, _) = send(
        &ctx.app,
        req(
            "POST",
            &items_url,
            &tok,
            Some(serde_json::json!({ "arrangementId": a1 })),
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
    assert_eq!(arr[0]["position"], 1);
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
            Some(serde_json::json!({ "arrangementId": arr, "position": 1 })),
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

/// Helper: create a collection and return its id.
async fn make_collection(ctx: &Ctx, tok: &str, name: &str, kind: &str) -> String {
    let (_s, coll) = send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections", ctx.org),
            tok,
            Some(serde_json::json!({ "name": name, "type": kind })),
        ),
    )
    .await;
    coll["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn reorder_rejects_an_incomplete_id_set() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let a1 = create_arrangement(&ctx.pool, ctx.org, "alpha").await;
    let a2 = create_arrangement(&ctx.pool, ctx.org, "beta").await;
    let coll = make_collection(&ctx, &tok, "Spring", "program").await;
    let items_url = format!("/v1/orgs/{}/collections/{}/items", ctx.org, coll);
    let (_s, i1) = send(
        &ctx.app,
        req(
            "POST",
            &items_url,
            &tok,
            Some(serde_json::json!({ "arrangementId": a1, "position": 1 })),
        ),
    )
    .await;
    send(
        &ctx.app,
        req(
            "POST",
            &items_url,
            &tok,
            Some(serde_json::json!({ "arrangementId": a2, "position": 2 })),
        ),
    )
    .await;
    let id1 = i1["id"].as_str().unwrap();

    // A partial list (only one of two live items) is rejected — no silent
    // corruption.
    let (status, _) = send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections/{}/reorder", ctx.org, coll),
            &tok,
            Some(serde_json::json!({ "orderedIds": [id1] })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);

    // The original order survived (indices still 1,2 — not parked at +1000000).
    let (_s, list) = send(&ctx.app, req("GET", &items_url, &tok, None)).await;
    let arr = list.as_array().unwrap();
    assert_eq!(arr[0]["position"], 1);
    assert_eq!(arr[1]["position"], 2);
}

#[tokio::test]
async fn undelete_collection_with_a_reused_slug_returns_409() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let base = format!("/v1/orgs/{}/collections", ctx.org);

    // Create, soft-delete, then re-create a collection with the same slug.
    let coll = make_collection(&ctx, &tok, "Spring", "program").await;
    let etag_resp = ctx
        .app
        .clone()
        .oneshot(req("GET", &format!("{base}/{coll}"), &tok, None))
        .await
        .unwrap();
    let etag = etag_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let del = axum::http::Request::builder()
        .method("DELETE")
        .uri(format!("{base}/{coll}"))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {tok}"))
        .header(axum::http::header::IF_MATCH, &etag)
        .body(axum::body::Body::empty())
        .unwrap();
    let (ds, _) = send(&ctx.app, del).await;
    assert_eq!(ds, axum::http::StatusCode::NO_CONTENT);

    make_collection(&ctx, &tok, "Spring", "program").await; // same slug, now live

    // Undeleting the first now clashes on the live-rows-only unique slug → 409.
    let (status, _) = send(
        &ctx.app,
        req("POST", &format!("{base}/{coll}/undelete"), &tok, None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn if_match_is_enforced_on_collection_update() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let base = format!("/v1/orgs/{}/collections", ctx.org);
    let coll = make_collection(&ctx, &tok, "Spring", "program").await;
    let url = format!("{base}/{coll}");
    let patch_body = serde_json::json!({ "name": "Spring Renamed", "type": "program" });

    // Missing If-Match → 412.
    let (missing, _) = send(&ctx.app, req("PATCH", &url, &tok, Some(patch_body.clone()))).await;
    assert_eq!(missing, axum::http::StatusCode::PRECONDITION_FAILED);

    // Stale If-Match → 412.
    let stale = axum::http::Request::builder()
        .method("PATCH")
        .uri(&url)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {tok}"))
        .header(axum::http::header::IF_MATCH, "\"1\"")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(patch_body.to_string()))
        .unwrap();
    let (stale_status, _) = send(&ctx.app, stale).await;
    assert_eq!(stale_status, axum::http::StatusCode::PRECONDITION_FAILED);

    // A fresh ETag from GET (create now emits it too) lets the PATCH through.
    let get = ctx
        .app
        .clone()
        .oneshot(req("GET", &url, &tok, None))
        .await
        .unwrap();
    let etag = get
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let ok = axum::http::Request::builder()
        .method("PATCH")
        .uri(&url)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {tok}"))
        .header(axum::http::header::IF_MATCH, &etag)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(patch_body.to_string()))
        .unwrap();
    let (ok_status, _) = send(&ctx.app, ok).await;
    assert_eq!(ok_status, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn a_musician_can_read_but_not_write() {
    let (ctx, editor) = setup(membership::Role::Archivist).await;
    let coll = make_collection(&ctx, &editor, "Spring", "program").await;

    // A second user in the same org, as a musician.
    let muso = create_user(&ctx.pool, "muso").await;
    add_member(&ctx.pool, ctx.org, muso, membership::Role::Musician).await;
    let muso_tok = ctx.token_for(muso);

    // Musician can list and get.
    let (list_status, _) = send(
        &ctx.app,
        req(
            "GET",
            &format!("/v1/orgs/{}/collections", ctx.org),
            &muso_tok,
            None,
        ),
    )
    .await;
    assert_eq!(list_status, axum::http::StatusCode::OK);
    let (get_status, _) = send(
        &ctx.app,
        req(
            "GET",
            &format!("/v1/orgs/{}/collections/{}", ctx.org, coll),
            &muso_tok,
            None,
        ),
    )
    .await;
    assert_eq!(get_status, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn an_out_of_range_position_is_rejected() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "alpha").await;
    let coll = make_collection(&ctx, &tok, "Spring", "program").await;
    let items_url = format!("/v1/orgs/{}/collections/{}/items", ctx.org, coll);

    // An empty program takes position 1 only.
    for bad in [0, -1, 2, 1_000_000] {
        let (status, _) = send(
            &ctx.app,
            req(
                "POST",
                &items_url,
                &tok,
                Some(serde_json::json!({ "arrangementId": arr, "position": bad })),
            ),
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::BAD_REQUEST,
            "position {bad} must be rejected"
        );
    }
}

#[tokio::test]
async fn collections_are_isolated_across_orgs() {
    let (ctx, _tok) = setup(membership::Role::Archivist).await;
    // A collection in org A.
    let coll_a = {
        let a_tok = {
            // The setup actor is an archivist in ctx.org (org A).
            let actor = create_user(&ctx.pool, "a-editor").await;
            add_member(&ctx.pool, ctx.org, actor, membership::Role::Archivist).await;
            ctx.token_for(actor)
        };
        make_collection(&ctx, &a_tok, "Program A", "program").await
    };

    // A second org B with its own editor.
    let org_b = create_org(&ctx.pool, "Beta Org").await;
    let b_editor = create_user(&ctx.pool, "b-editor").await;
    add_member(&ctx.pool, org_b, b_editor, membership::Role::Archivist).await;
    let b_tok = ctx.token_for(b_editor);

    // Org B's editor is not a member of org A → 403 on A's collection.
    let (status, _) = send(
        &ctx.app,
        req(
            "GET",
            &format!("/v1/orgs/{}/collections/{}", ctx.org, coll_a),
            &b_tok,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

    // Adding an arrangement from org A to a collection in org B → 404.
    let arr_a = create_arrangement(&ctx.pool, ctx.org, "a-piece").await;
    let coll_b = make_collection_in(&ctx, &b_tok, org_b, "Program B", "program").await;
    let (add_status, _) = send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{}/collections/{}/items", org_b, coll_b),
            &b_tok,
            Some(serde_json::json!({ "arrangementId": arr_a, "position": 1 })),
        ),
    )
    .await;
    assert_eq!(add_status, axum::http::StatusCode::NOT_FOUND);
}

/// Like `make_collection` but in an explicit org.
async fn make_collection_in(ctx: &Ctx, tok: &str, org: Uuid, name: &str, kind: &str) -> String {
    let (_s, coll) = send(
        &ctx.app,
        req(
            "POST",
            &format!("/v1/orgs/{org}/collections"),
            tok,
            Some(serde_json::json!({ "name": name, "type": kind })),
        ),
    )
    .await;
    coll["id"].as_str().unwrap().to_string()
}

/// PATCH `url` with its current ETag; returns status, response ETag and body.
async fn patch_current(
    ctx: &Ctx,
    tok: &str,
    url: &str,
    body: serde_json::Value,
) -> (axum::http::StatusCode, String, serde_json::Value) {
    let get = ctx
        .app
        .clone()
        .oneshot(req("GET", url, tok, None))
        .await
        .unwrap();
    let etag = get.headers()["etag"].to_str().unwrap().to_string();
    let request = axum::http::Request::builder()
        .method("PATCH")
        .uri(url)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {tok}"))
        .header(axum::http::header::IF_MATCH, &etag)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let resp = ctx.app.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let new_etag = resp
        .headers()
        .get("etag")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, new_etag, json)
}

#[tokio::test]
async fn collection_and_item_patches_are_partial() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let coll = make_collection(&ctx, &tok, "Spring", "program").await;
    let url = format!("/v1/orgs/{}/collections/{coll}", ctx.org);

    let (status, _, body) = patch_current(
        &ctx,
        &tok,
        &url,
        serde_json::json!({ "name": "Spring Gala" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["name"], "Spring Gala");
    assert_eq!(body["type"], "program");

    let (status, _, body) =
        patch_current(&ctx, &tok, &url, serde_json::json!({ "type": "standing" })).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["name"], "Spring Gala");
    assert_eq!(body["type"], "standing");

    let (status, _, _) =
        patch_current(&ctx, &tok, &url, serde_json::json!({ "type": "concert" })).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

    // An item PATCH that does not move it writes nothing.
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let (status, item) = send(
        &ctx.app,
        req(
            "POST",
            &format!("{url}/items"),
            &tok,
            // The collection is standing by now: its pieces take labels.
            Some(serde_json::json!({ "arrangementId": arr, "label": "1" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let item_url = format!("{url}/items/{}", item["id"].as_str().unwrap());
    let (_, first, _) = patch_current(&ctx, &tok, &item_url, serde_json::json!({})).await;
    let (status, second, body) =
        patch_current(&ctx, &tok, &item_url, serde_json::json!({ "label": "1" })).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["label"], "1");
    assert_eq!(first, second, "an unchanged item keeps its ETag");
}

// ---------------------------------------------------------------------------
// Collection numbering (#102; docs/design.md, *Collection numbering*)
// ---------------------------------------------------------------------------

use lied::domain::collection_item::{CollectionItemError, LabelRefusal, Placement};
use lied::domain::{collection, collection_item};

/// A collection of `kind` with `width`-digit numbers, created through the
/// domain.
async fn numbered(ctx: &Ctx, kind: &str, width: i16) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("{kind}-{id}");
    collection::create(
        &ctx.pool,
        id,
        ctx.org,
        &name,
        &collection::slugify(&name),
        kind,
        width,
        None,
    )
    .await
    .expect("collection");
    id
}

/// Add a new arrangement to `coll` at `placement`.
async fn add(
    ctx: &Ctx,
    coll: Uuid,
    title: &str,
    placement: Placement<'_>,
) -> Result<collection_item::CollectionItem, CollectionItemError> {
    let slug = format!("{title}-{}", Uuid::now_v7());
    let arr = create_arrangement(&ctx.pool, ctx.org, &slug).await;
    collection_item::create(&ctx.pool, Uuid::now_v7(), coll, arr, placement, None).await
}

fn at(label: &str) -> Placement<'_> {
    Placement {
        position: None,
        label: Some(label),
    }
}

fn pos(position: i32) -> Placement<'static> {
    Placement {
        position: Some(position),
        label: None,
    }
}

/// `(label, position)` of the live items, in position order.
async fn layout(ctx: &Ctx, coll: Uuid) -> Vec<(String, i32)> {
    collection_item::list_for_collection(&ctx.pool, coll)
        .await
        .unwrap()
        .into_iter()
        .map(|v| (v.item.label, v.item.position))
        .collect()
}

/// The live items' labels, in position order.
async fn labels(ctx: &Ctx, coll: Uuid) -> Vec<String> {
    layout(ctx, coll)
        .await
        .into_iter()
        .map(|(l, _)| l)
        .collect()
}

/// The live items' ids, in position order.
async fn order(ctx: &Ctx, coll: Uuid) -> Vec<Uuid> {
    collection_item::list_for_collection(&ctx.pool, coll)
        .await
        .unwrap()
        .iter()
        .map(|v| v.item.id)
        .collect()
}

fn refusal<T: std::fmt::Debug>(result: Result<T, CollectionItemError>) -> LabelRefusal {
    match result {
        Err(CollectionItemError::LabelRefused { refusal, .. }) => refusal,
        other => panic!("expected a label refusal, got {other:?}"),
    }
}

fn owned(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|l| l.to_string()).collect()
}

#[tokio::test]
async fn a_standing_collection_keeps_its_gaps_and_takes_allowed_labels_only() {
    let (ctx, _tok) = setup(membership::Role::Archivist).await;
    let book = numbered(&ctx, "standing", 3).await;

    let mut ids = Vec::new();
    for title in ["a", "b", "c"] {
        ids.push(
            add(&ctx, book, title, Placement::default())
                .await
                .unwrap()
                .id,
        );
    }
    assert_eq!(
        labels(&ctx, book).await,
        owned(&["1", "2", "3"]),
        "next number by default"
    );

    // Removing 2 leaves the gap; positions stay dense.
    assert!(collection_item::remove(&ctx.pool, book, ids[1], None)
        .await
        .unwrap());
    assert_eq!(
        layout(&ctx, book).await,
        [("1".to_string(), 1), ("3".to_string(), 2)]
    );

    // The gap can be filled, a letter slipped in after a used number, and
    // positions follow the natural order of the labels.
    add(&ctx, book, "d", at("2")).await.unwrap();
    let slipped = add(&ctx, book, "e", at("1A")).await.unwrap();
    assert_eq!(slipped.label, "1a", "labels are stored lowercase");
    add(&ctx, book, "f", at("3b")).await.unwrap();
    assert_eq!(
        layout(&ctx, book).await,
        [
            ("1".to_string(), 1),
            ("1a".to_string(), 2),
            ("2".to_string(), 3),
            ("3".to_string(), 4),
            ("3b".to_string(), 5),
        ]
    );

    // Refusals.
    assert_eq!(
        refusal(add(&ctx, book, "g", at("5")).await),
        LabelRefusal::NotAllowed,
        "past the end"
    );
    assert_eq!(
        refusal(add(&ctx, book, "g", at("3")).await),
        LabelRefusal::Taken
    );
    assert_eq!(
        refusal(add(&ctx, book, "g", at("4a")).await),
        LabelRefusal::NotAllowed,
        "no piece is numbered 4"
    );
    assert_eq!(
        refusal(add(&ctx, book, "g", at("1000")).await),
        LabelRefusal::TooWide
    );
    assert!(matches!(
        add(&ctx, book, "g", at("07")).await,
        Err(CollectionItemError::InvalidLabel)
    ));
    assert!(matches!(
        add(&ctx, book, "g", pos(1)).await,
        Err(CollectionItemError::WrongField {
            field: "position",
            ..
        })
    ));
    // The next number stays allowed.
    assert_eq!(add(&ctx, book, "g", at("4")).await.unwrap().label, "4");
}

#[tokio::test]
async fn a_program_renumbers_freely() {
    let (ctx, _tok) = setup(membership::Role::Archivist).await;
    let concert = numbered(&ctx, "program", 3).await;
    let a = add(&ctx, concert, "a", Placement::default()).await.unwrap();
    let b = add(&ctx, concert, "b", Placement::default()).await.unwrap();
    // Insert at the front: the rest move down, and labels are positions.
    let c = add(&ctx, concert, "c", pos(1)).await.unwrap();
    assert_eq!(
        layout(&ctx, concert).await,
        [
            ("1".to_string(), 1),
            ("2".to_string(), 2),
            ("3".to_string(), 3)
        ]
    );
    assert_eq!(order(&ctx, concert).await, [c.id, a.id, b.id]);

    // Removing closes the gap.
    assert!(collection_item::remove(&ctx.pool, concert, a.id, None)
        .await
        .unwrap());
    assert_eq!(labels(&ctx, concert).await, owned(&["1", "2"]));
    assert_eq!(order(&ctx, concert).await, [c.id, b.id]);

    // Moving shifts the pieces in between.
    let moved = collection_item::move_item(&ctx.pool, concert, b.id, None, pos(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((moved.position, moved.label.as_str()), (1, "1"));
    assert_eq!(order(&ctx, concert).await, [b.id, c.id]);

    // Out of range, and the wrong field for a program.
    assert!(matches!(
        add(&ctx, concert, "d", pos(4)).await,
        Err(CollectionItemError::InvalidPosition { max: 3 })
    ));
    assert!(matches!(
        collection_item::move_item(&ctx.pool, concert, b.id, None, pos(3)).await,
        Err(CollectionItemError::InvalidPosition { max: 2 })
    ));
    assert!(matches!(
        add(&ctx, concert, "d", at("3")).await,
        Err(CollectionItemError::WrongField { field: "label", .. })
    ));

    // Restoring puts a piece back at its old position (a was 2nd).
    assert!(collection_item::restore(&ctx.pool, concert, a.id, None)
        .await
        .unwrap());
    assert_eq!(order(&ctx, concert).await, [b.id, a.id, c.id]);
    assert_eq!(labels(&ctx, concert).await, owned(&["1", "2", "3"]));

    // Swapping exchanges positions.
    assert!(collection_item::swap(&ctx.pool, concert, b.id, c.id)
        .await
        .unwrap());
    assert_eq!(order(&ctx, concert).await, [c.id, a.id, b.id]);
}

#[tokio::test]
async fn a_standing_piece_moves_to_a_free_label_or_swaps() {
    let (ctx, _tok) = setup(membership::Role::Archivist).await;
    let book = numbered(&ctx, "standing", 3).await;
    let one = add(&ctx, book, "a", Placement::default()).await.unwrap();
    let two = add(&ctx, book, "b", Placement::default()).await.unwrap();
    let three = add(&ctx, book, "c", Placement::default()).await.unwrap();
    let four = add(&ctx, book, "d", Placement::default()).await.unwrap();
    assert!(collection_item::remove(&ctx.pool, book, two.id, None)
        .await
        .unwrap());

    // Into the gap: nothing else changes number.
    let moved = collection_item::move_item(&ctx.pool, book, four.id, None, at("2"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.label, "2");
    assert_eq!(labels(&ctx, book).await, owned(&["1", "2", "3"]));
    assert_eq!(order(&ctx, book).await, [one.id, four.id, three.id]);

    // Past the end is refused: moving 3, the highest, to 4 would invent a
    // gap at 3 (its own label does not count for it).
    assert_eq!(
        refusal(collection_item::move_item(&ctx.pool, book, three.id, None, at("4")).await),
        LabelRefusal::NotAllowed
    );
    assert_eq!(
        refusal(collection_item::move_item(&ctx.pool, book, one.id, None, at("3")).await),
        LabelRefusal::Taken
    );
    assert!(matches!(
        collection_item::move_item(&ctx.pool, book, one.id, None, pos(2)).await,
        Err(CollectionItemError::WrongField {
            field: "position",
            ..
        })
    ));

    // Swap exchanges the labels; the order follows them.
    assert!(collection_item::swap(&ctx.pool, book, one.id, three.id)
        .await
        .unwrap());
    assert_eq!(order(&ctx, book).await, [three.id, four.id, one.id]);
    assert_eq!(labels(&ctx, book).await, owned(&["1", "2", "3"]));
    assert!(matches!(
        collection_item::swap(&ctx.pool, book, one.id, one.id).await,
        Err(CollectionItemError::SameItem)
    ));
    // Reordering is for programs.
    assert!(matches!(
        collection_item::reorder(&ctx.pool, book, &[one.id, three.id, four.id]).await,
        Err(CollectionItemError::NotProgram)
    ));
}

#[tokio::test]
async fn a_standing_piece_is_restored_under_its_old_label_or_a_chosen_one() {
    let (ctx, _tok) = setup(membership::Role::Archivist).await;
    let book = numbered(&ctx, "standing", 3).await;
    add(&ctx, book, "a", Placement::default()).await.unwrap();
    let two = add(&ctx, book, "b", Placement::default()).await.unwrap();
    let three = add(&ctx, book, "c", Placement::default()).await.unwrap();

    // Old label free: it comes back there, even when that is now past the
    // end (restoring is an undo, not a new number).
    assert!(collection_item::remove(&ctx.pool, book, three.id, None)
        .await
        .unwrap());
    assert!(collection_item::remove(&ctx.pool, book, two.id, None)
        .await
        .unwrap());
    assert!(collection_item::restore(&ctx.pool, book, three.id, None)
        .await
        .unwrap());
    assert_eq!(labels(&ctx, book).await, owned(&["1", "3"]));

    // Old label taken: a label is needed, and it must be an allowed one.
    add(&ctx, book, "d", at("2")).await.unwrap();
    assert!(matches!(
        collection_item::restore(&ctx.pool, book, two.id, None).await,
        Err(CollectionItemError::LabelNeeded { ref old }) if old == "2"
    ));
    assert_eq!(
        refusal(collection_item::restore(&ctx.pool, book, two.id, Some("9")).await),
        LabelRefusal::NotAllowed
    );
    assert!(collection_item::restore(&ctx.pool, book, two.id, Some("4"))
        .await
        .unwrap());
    assert_eq!(labels(&ctx, book).await, owned(&["1", "2", "3", "4"]));
}

#[tokio::test]
async fn renumber_closes_the_gaps_only_for_the_previewed_state() {
    let (ctx, _tok) = setup(membership::Role::Archivist).await;
    let book = numbered(&ctx, "standing", 3).await;
    let mut ids = Vec::new();
    for title in ["a", "b", "c", "d"] {
        ids.push(
            add(&ctx, book, title, Placement::default())
                .await
                .unwrap()
                .id,
        );
    }
    add(&ctx, book, "e", at("4a")).await.unwrap();
    assert!(collection_item::remove(&ctx.pool, book, ids[1], None)
        .await
        .unwrap());
    assert_eq!(labels(&ctx, book).await, owned(&["1", "3", "4", "4a"]));

    let preview = collection_item::renumber_preview(&ctx.pool, book)
        .await
        .unwrap();
    let pairs: Vec<(&str, &str)> = preview
        .changes
        .iter()
        .map(|c| (c.from.as_str(), c.to.as_str()))
        .collect();
    assert_eq!(pairs, [("1", "1"), ("3", "2"), ("4", "3"), ("4a", "4")]);
    assert_eq!(
        labels(&ctx, book).await,
        owned(&["1", "3", "4", "4a"]),
        "a preview changes nothing"
    );

    // The book changes after the preview: the old token no longer applies.
    add(&ctx, book, "f", at("2")).await.unwrap();
    assert!(matches!(
        collection_item::renumber(&ctx.pool, book, &preview.preview_token).await,
        Err(CollectionItemError::StaleRenumber)
    ));
    assert_eq!(labels(&ctx, book).await, owned(&["1", "2", "3", "4", "4a"]));

    let fresh = collection_item::renumber_preview(&ctx.pool, book)
        .await
        .unwrap();
    let changes = collection_item::renumber(&ctx.pool, book, &fresh.preview_token)
        .await
        .unwrap();
    assert_eq!(changes, fresh.changes, "apply does what the preview showed");
    assert_eq!(labels(&ctx, book).await, owned(&["1", "2", "3", "4", "5"]));

    // Programs are always dense: nothing to renumber.
    let concert = numbered(&ctx, "program", 3).await;
    assert!(matches!(
        collection_item::renumber_preview(&ctx.pool, concert).await,
        Err(CollectionItemError::NotStanding)
    ));
}

#[tokio::test]
async fn type_and_width_changes_keep_the_numbers_consistent() {
    let (ctx, _tok) = setup(membership::Role::Archivist).await;
    let book = numbered(&ctx, "standing", 4).await;
    let mut ids = Vec::new();
    for title in ["a", "b", "c"] {
        ids.push(
            add(&ctx, book, title, Placement::default())
                .await
                .unwrap()
                .id,
        );
    }
    add(&ctx, book, "d", at("3a")).await.unwrap();
    assert!(collection_item::remove(&ctx.pool, book, ids[0], None)
        .await
        .unwrap());
    assert_eq!(labels(&ctx, book).await, owned(&["2", "3", "3a"]));

    let fields = |collection_type, number_width| collection::CollectionFields {
        name: "Book",
        collection_type,
        number_width,
    };
    // Narrowing is fine while every label fits.
    let narrowed = collection::update(&ctx.pool, book, None, fields("standing", 3))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(narrowed.number_width, 3);

    // Standing → program closes the gaps; program → standing keeps them.
    collection::update(&ctx.pool, book, None, fields("program", 3))
        .await
        .unwrap();
    assert_eq!(labels(&ctx, book).await, owned(&["1", "2", "3"]));
    collection::update(&ctx.pool, book, None, fields("standing", 3))
        .await
        .unwrap();
    assert_eq!(labels(&ctx, book).await, owned(&["1", "2", "3"]));

    // A four-digit label blocks narrowing to three digits.
    let wide = numbered(&ctx, "standing", 4).await;
    let big = create_arrangement(&ctx.pool, ctx.org, "big").await;
    sqlx::query(
        "INSERT INTO collection_item (id, collection_id, arrangement_id, position, label) \
         VALUES ($1, $2, $3, 1, '1000')",
    )
    .bind(Uuid::now_v7())
    .bind(wide)
    .bind(big)
    .execute(&ctx.pool)
    .await
    .unwrap();
    assert!(matches!(
        collection::update(&ctx.pool, wide, None, fields("standing", 3)).await,
        Err(collection::CollectionError::LabelsTooWide {
            highest: 1000,
            width: 3
        })
    ));
    // And the next number there is 1001.
    assert_eq!(
        add(&ctx, wide, "next", Placement::default())
            .await
            .unwrap()
            .label,
        "1001"
    );
}

#[tokio::test]
async fn concurrent_adds_under_one_label_let_exactly_one_through() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let book = make_collection(&ctx, &tok, "March Book", "standing").await;
    let items_url = format!("/v1/orgs/{}/collections/{book}/items", ctx.org);
    for round in 0..5 {
        let a = create_arrangement(&ctx.pool, ctx.org, &format!("a{round}")).await;
        let b = create_arrangement(&ctx.pool, ctx.org, &format!("b{round}")).await;
        let label = (round + 1).to_string();
        let body = |arr: Uuid| Some(serde_json::json!({ "arrangementId": arr, "label": label }));
        let ((s1, _), (s2, _)) = tokio::join!(
            send(&ctx.app, req("POST", &items_url, &tok, body(a))),
            send(&ctx.app, req("POST", &items_url, &tok, body(b))),
        );
        let mut statuses = [s1, s2];
        statuses.sort();
        assert_eq!(
            statuses,
            [
                axum::http::StatusCode::CREATED,
                axum::http::StatusCode::CONFLICT
            ],
            "round {round}"
        );
    }
    let book: Uuid = book.parse().unwrap();
    assert_eq!(labels(&ctx, book).await, owned(&["1", "2", "3", "4", "5"]));
}

#[tokio::test]
async fn the_wire_carries_positions_labels_and_the_number_width() {
    let (ctx, tok) = setup(membership::Role::Archivist).await;
    let base = format!("/v1/orgs/{}/collections", ctx.org);
    let create = |body: serde_json::Value| req("POST", &base, &tok, Some(body));
    let (status, book) = send(
        &ctx.app,
        create(serde_json::json!({ "name": "Big Book", "type": "standing", "numberWidth": 4 })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(book["numberWidth"], 4);
    let (status, problem) = send(
        &ctx.app,
        create(serde_json::json!({ "name": "Odd", "type": "standing", "numberWidth": 5 })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(problem["errors"].get("numberWidth").is_some(), "{problem}");

    let book_url = format!("{base}/{}", book["id"].as_str().unwrap());
    let items_url = format!("{book_url}/items");
    let mut items = Vec::new();
    for slug in ["alpha", "beta", "gamma"] {
        let arr = create_arrangement(&ctx.pool, ctx.org, slug).await;
        let (status, item) = send(
            &ctx.app,
            req(
                "POST",
                &items_url,
                &tok,
                Some(serde_json::json!({ "arrangementId": arr })),
            ),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::CREATED);
        assert!(item.get("index").is_none(), "{item}");
        items.push(item);
    }
    assert_eq!(items[2]["label"], "3");
    assert_eq!(items[2]["position"], 3);

    // The wrong field for the type is a 400 naming it; a label past the end
    // a 400 naming `label`; a taken one a 409.
    let arr = create_arrangement(&ctx.pool, ctx.org, "delta").await;
    for (body, want, field) in [
        (
            serde_json::json!({ "arrangementId": arr, "position": 1 }),
            axum::http::StatusCode::BAD_REQUEST,
            Some("position"),
        ),
        (
            serde_json::json!({ "arrangementId": arr, "label": "9" }),
            axum::http::StatusCode::BAD_REQUEST,
            Some("label"),
        ),
        (
            serde_json::json!({ "arrangementId": arr, "label": "2" }),
            axum::http::StatusCode::CONFLICT,
            None,
        ),
    ] {
        let (status, problem) = send(&ctx.app, req("POST", &items_url, &tok, Some(body))).await;
        assert_eq!(status, want, "{problem}");
        if let Some(field) = field {
            assert!(problem["errors"].get(field).is_some(), "{problem}");
        }
    }

    // Remove 2, then restore it without a body: it comes back under 2.
    let two = items[1]["id"].as_str().unwrap();
    collection_item::remove(
        &ctx.pool,
        book["id"].as_str().unwrap().parse().unwrap(),
        two.parse().unwrap(),
        None,
    )
    .await
    .unwrap();
    let (status, _) = send(
        &ctx.app,
        req("POST", &format!("{items_url}/{two}/undelete"), &tok, None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    // Swap 1 and 3.
    let (one, three) = (
        items[0]["id"].as_str().unwrap(),
        items[2]["id"].as_str().unwrap(),
    );
    let (status, _) = send(
        &ctx.app,
        req(
            "POST",
            &format!("{items_url}/{one}/swap"),
            &tok,
            Some(serde_json::json!({ "withItemId": three })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (_, list) = send(&ctx.app, req("GET", &items_url, &tok, None)).await;
    let rows: Vec<(&str, &str)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["label"].as_str().unwrap(),
                i["arrangementSlug"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(rows, [("1", "gamma"), ("2", "beta"), ("3", "alpha")]);

    // Renumber: preview, then apply with the token; a stale token is a 409.
    let renumber_url = format!("{book_url}/renumber");
    let (status, preview) = send(&ctx.app, req("GET", &renumber_url, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(preview["changes"].as_array().unwrap().len(), 3);
    let apply = |token: serde_json::Value| {
        req(
            "POST",
            &renumber_url,
            &tok,
            Some(serde_json::json!({ "previewToken": token })),
        )
    };
    let (status, _) = send(&ctx.app, apply(serde_json::json!("stale"))).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    let (status, applied) = send(&ctx.app, apply(preview["previewToken"].clone())).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(applied["changes"], preview["changes"]);

    // Reorder is for programs.
    let (status, _) = send(
        &ctx.app,
        req(
            "POST",
            &format!("{book_url}/reorder"),
            &tok,
            Some(serde_json::json!({ "orderedIds": [three, two, one] })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
}
