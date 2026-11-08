//! Integration tests for issue #36: global annotations over `/v1` — CRUD,
//! the permission matrix (owner/conductor write, staff read), ETag/If-Match,
//! hard delete + audit, org/arrangement scoping, and hide-with-references
//! under a soft-deleted arrangement. Real Postgres via testcontainers.

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
        jwt_signing_key: Secret::from("test-signing-key-material-for-annotation-tests".to_string()),
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

/// An arrangement, through the real repository function.
async fn create_arrangement(pool: &PgPool, org_id: Uuid, title: &str) -> Uuid {
    let id = Uuid::now_v7();
    lied::domain::arrangement::create(
        pool,
        id,
        org_id,
        &lied::domain::arrangement::slugify(title),
        lied::domain::arrangement::ArrangementFields {
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

fn list_url(org: Uuid, arr: Uuid) -> String {
    format!("/v1/orgs/{org}/arrangements/{arr}/annotations")
}

/// Create an annotation as `tok`, asserting 201, returning (body, etag).
async fn create(
    ctx: &Ctx,
    tok: &str,
    arr: Uuid,
    annotation_type: &str,
    content: &str,
) -> (serde_json::Value, String) {
    let (status, body, headers) = send(
        &ctx.app,
        req(
            "POST",
            &list_url(ctx.org, arr),
            tok,
            Some(serde_json::json!({ "type": annotation_type, "content": content })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let etag = headers
        .get("etag")
        .expect("create returns an ETag")
        .to_str()
        .unwrap()
        .to_string();
    (body, etag)
}

async fn audit_rows(pool: &PgPool, action: &str, target: Uuid) -> Vec<serde_json::Value> {
    sqlx::query_scalar!(
        r#"SELECT payload as "payload!" FROM audit_log WHERE action = $1 AND target_id = $2 ORDER BY at, id"#,
        action,
        target
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A member of `ctx.org` with `role`, and a token for them.
async fn member(ctx: &Ctx, username: &str, role: membership::Role) -> (Uuid, String) {
    let user = create_user(&ctx.pool, username).await;
    add_member(&ctx.pool, ctx.org, user, role).await;
    (user, ctx.token_for(user))
}

#[tokio::test]
async fn a_conductor_writes_reads_edits_and_hard_deletes_an_annotation() {
    let (ctx, tok, conductor) = setup(membership::Role::Conductor).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;

    let (created, etag) = create(&ctx, &tok, arr, "tempo", "bar 12: poco rit.").await;
    assert_eq!(created["type"], "tempo");
    assert_eq!(created["content"], "bar 12: poco rit.");
    assert_eq!(created["authorId"], conductor.to_string());
    assert_eq!(created["authorDisplayName"], "actor");
    let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let url = format!("{}/{id}", list_url(ctx.org, arr));

    // Listed in an envelope, and readable by id with the same ETag.
    create(&ctx, &tok, arr, "structure", "cut bars 40–56").await;
    let (status, page, _) = send(&ctx.app, req("GET", &list_url(ctx.org, arr), &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(page["total"], 2);
    assert_eq!(
        page["items"][0]["id"],
        id.to_string(),
        "oldest first by default"
    );
    let (status, got, headers) = send(&ctx.app, req("GET", &url, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(got["content"], "bar 12: poco rit.");
    assert_eq!(headers.get("etag").unwrap().to_str().unwrap(), etag);

    // PATCH is partial: changing the type leaves the content alone.
    let (status, updated, headers) = send(
        &ctx.app,
        req_with_if_match(
            "PATCH",
            &url,
            &tok,
            Some(&etag),
            Some(serde_json::json!({ "type": "dynamics" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{updated}");
    assert_eq!(updated["type"], "dynamics");
    assert_eq!(updated["content"], "bar 12: poco rit.");
    let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();

    // ...and changing only the content leaves the type alone.
    let (status, updated, headers) = send(
        &ctx.app,
        req_with_if_match(
            "PATCH",
            &url,
            &tok,
            Some(&etag),
            Some(serde_json::json!({ "content": "bar 12: molto rit." })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{updated}");
    assert_eq!(updated["type"], "dynamics");
    assert_eq!(updated["content"], "bar 12: molto rit.");
    let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();

    // DELETE removes the row for good.
    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match("DELETE", &url, &tok, Some(&etag), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (status, _, _) = send(&ctx.app, req("GET", &url, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let remaining: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "n!" FROM global_annotation WHERE id = $1"#,
        id
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0, "a hard delete, not a hidden row");

    // Every write audited; the delete keeps the content it removed.
    assert_eq!(
        audit_rows(&ctx.pool, "global_annotation.create", id)
            .await
            .len(),
        1
    );
    let update = audit_rows(&ctx.pool, "global_annotation.update", id).await;
    assert_eq!(update.len(), 2);
    assert_eq!(update[0]["before"]["type"], "tempo");
    assert_eq!(update[0]["after"]["type"], "dynamics");
    let delete = audit_rows(&ctx.pool, "global_annotation.delete", id).await;
    assert_eq!(delete.len(), 1);
    assert_eq!(delete[0]["hardDelete"], true);
    assert_eq!(delete[0]["content"], "bar 12: molto rit.");
}

#[tokio::test]
async fn owner_and_conductor_write_archivist_reads_and_a_musician_sees_nothing() {
    let (ctx, owner_tok, _) = setup(membership::Role::Owner).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let (note, etag) = create(&ctx, &owner_tok, arr, "general", "owner's note").await;
    let url = format!(
        "{}/{}",
        list_url(ctx.org, arr),
        note["id"].as_str().unwrap()
    );

    let (_, conductor_tok) = member(&ctx, "conductor", membership::Role::Conductor).await;
    create(&ctx, &conductor_tok, arr, "tempo", "conductor's note").await;

    // Archivist: reads both, writes nothing.
    let (_, archivist_tok) = member(&ctx, "archivist", membership::Role::Archivist).await;
    let (status, page, _) = send(
        &ctx.app,
        req("GET", &list_url(ctx.org, arr), &archivist_tok, None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(page["total"], 2);
    let (status, _, _) = send(&ctx.app, req("GET", &url, &archivist_tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let (status, _, _) = send(
        &ctx.app,
        req(
            "POST",
            &list_url(ctx.org, arr),
            &archivist_tok,
            Some(serde_json::json!({ "type": "general", "content": "x" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match(
            "PATCH",
            &url,
            &archivist_tok,
            Some(&etag),
            Some(serde_json::json!({ "content": "x" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match("DELETE", &url, &archivist_tok, Some(&etag), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

    // Musician: no access at all for now (consumption is the phase-3 client).
    let (_, musician_tok) = member(&ctx, "musician", membership::Role::Musician).await;
    for request in [
        req("GET", &list_url(ctx.org, arr), &musician_tok, None),
        req("GET", &url, &musician_tok, None),
    ] {
        let (status, _, _) = send(&ctx.app, request).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }

    // The owner's note is untouched by all of that.
    let (_, got, _) = send(&ctx.app, req("GET", &url, &owner_tok, None)).await;
    assert_eq!(got["content"], "owner's note");
}

#[tokio::test]
async fn a_stale_or_missing_if_match_is_a_412() {
    let (ctx, tok, _) = setup(membership::Role::Conductor).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let (note, etag) = create(&ctx, &tok, arr, "tempo", "first").await;
    let url = format!(
        "{}/{}",
        list_url(ctx.org, arr),
        note["id"].as_str().unwrap()
    );

    // Someone else edits it first.
    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match(
            "PATCH",
            &url,
            &tok,
            Some(&etag),
            Some(serde_json::json!({ "content": "second" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    for (method, body) in [
        (
            "PATCH",
            Some(serde_json::json!({ "content": "lost update" })),
        ),
        ("DELETE", None),
    ] {
        let (status, _, _) = send(
            &ctx.app,
            req_with_if_match(method, &url, &tok, Some(&etag), body.clone()),
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::PRECONDITION_FAILED,
            "{method} stale"
        );
        let (status, _, _) =
            send(&ctx.app, req_with_if_match(method, &url, &tok, None, body)).await;
        assert_eq!(
            status,
            axum::http::StatusCode::PRECONDITION_FAILED,
            "{method} missing"
        );
    }
    let (_, got, _) = send(&ctx.app, req("GET", &url, &tok, None)).await;
    assert_eq!(got["content"], "second");
}

#[tokio::test]
async fn an_unknown_type_or_empty_content_is_a_400_and_the_type_filter_works() {
    let (ctx, tok, _) = setup(membership::Role::Conductor).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    for (body, field) in [
        (serde_json::json!({ "type": "cut", "content": "x" }), "type"),
        (
            serde_json::json!({ "type": "tempo", "content": "   " }),
            "content",
        ),
    ] {
        let (status, problem, _) = send(
            &ctx.app,
            req("POST", &list_url(ctx.org, arr), &tok, Some(body)),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(problem["errors"][field].is_array(), "{problem}");
    }

    let (note, etag) = create(&ctx, &tok, arr, "tempo", "a").await;
    create(&ctx, &tok, arr, "structure", "b").await;
    let url = format!(
        "{}/{}",
        list_url(ctx.org, arr),
        note["id"].as_str().unwrap()
    );
    let (status, problem, _) = send(
        &ctx.app,
        req_with_if_match(
            "PATCH",
            &url,
            &tok,
            Some(&etag),
            Some(serde_json::json!({ "content": "" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{problem}");

    let (status, page, _) = send(
        &ctx.app,
        req(
            "GET",
            &format!("{}?filter%5Btype%5D=structure", list_url(ctx.org, arr)),
            &tok,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["content"], "b");
    let (status, _, _) = send(
        &ctx.app,
        req(
            "GET",
            &format!("{}?filter[type]=cut", list_url(ctx.org, arr)),
            &tok,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);

    // The vocabulary is enforced by the database too, not only by the API.
    let direct = sqlx::query!(
        "UPDATE global_annotation SET type = 'cut' WHERE id = $1",
        note["id"].as_str().unwrap().parse::<Uuid>().unwrap()
    )
    .execute(&ctx.pool)
    .await;
    assert!(
        direct.is_err(),
        "the CHECK constraint refuses an unknown type"
    );
}

#[tokio::test]
async fn an_annotation_is_only_reachable_through_its_own_org_and_arrangement() {
    let (ctx, tok, _) = setup(membership::Role::Conductor).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let other_arr = create_arrangement(&ctx.pool, ctx.org, "egmont").await;
    let (note, etag) = create(&ctx, &tok, arr, "tempo", "mine").await;
    let id = note["id"].as_str().unwrap();

    // The right id under the wrong arrangement.
    let wrong_arr = format!("{}/{id}", list_url(ctx.org, other_arr));
    let (status, _, _) = send(&ctx.app, req("GET", &wrong_arr, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match("DELETE", &wrong_arr, &tok, Some(&etag), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // A conductor of another org, through their own org's path.
    let rival = create_org(&ctx.pool, "Rival").await;
    let outsider = create_user(&ctx.pool, "outsider").await;
    add_member(&ctx.pool, rival, outsider, membership::Role::Conductor).await;
    let otok = ctx.token_for(outsider);
    let foreign = format!("/v1/orgs/{rival}/arrangements/{arr}/annotations");
    let (status, _, _) = send(&ctx.app, req("GET", &foreign, &otok, None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match(
            "DELETE",
            &format!("{foreign}/{id}"),
            &otok,
            Some(&etag),
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (status, _, _) = send(
        &ctx.app,
        req(
            "POST",
            &foreign,
            &otok,
            Some(serde_json::json!({ "type": "general", "content": "x" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    let (status, _, _) = send(
        &ctx.app,
        req(
            "GET",
            &format!("{}/{id}", list_url(ctx.org, arr)),
            &tok,
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "still there for its own org"
    );
}

#[tokio::test]
async fn a_removed_arrangement_hides_its_annotations_until_restored() {
    let (ctx, tok, _) = setup(membership::Role::Conductor).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let (note, etag) = create(&ctx, &tok, arr, "tempo", "kept").await;
    let url = format!(
        "{}/{}",
        list_url(ctx.org, arr),
        note["id"].as_str().unwrap()
    );

    assert!(lied::domain::arrangement::soft_delete(&ctx.pool, arr)
        .await
        .unwrap());
    let (status, _, _) = send(&ctx.app, req("GET", &url, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (status, _, _) = send(&ctx.app, req("GET", &list_url(ctx.org, arr), &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (status, _, _) = send(
        &ctx.app,
        req(
            "POST",
            &list_url(ctx.org, arr),
            &tok,
            Some(serde_json::json!({ "type": "general", "content": "x" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match("DELETE", &url, &tok, Some(&etag), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // Hide-with-references: restoring brings it back, unchanged.
    assert!(lied::domain::arrangement::undelete(&ctx.pool, arr)
        .await
        .unwrap());
    let (status, got, _) = send(&ctx.app, req("GET", &url, &tok, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(got["content"], "kept");
}

// ── review of #36 ───────────────────────────────────────────────────────────

/// The current version (ETag milliseconds) of an annotation.
async fn version_of(ctx: &Ctx, arr: Uuid, id: Uuid) -> i64 {
    lied::domain::global_annotation::find(&ctx.pool, ctx.org, arr, id)
        .await
        .unwrap()
        .expect("annotation")
        .updated_at
        .timestamp_millis()
}

/// Finding 1: the version check must be part of the write. Two writers holding
/// the same version cannot both succeed — the domain write is called directly
/// here, so the route's early check cannot be what stops the second.
#[tokio::test]
async fn two_writes_with_the_same_version_cannot_both_succeed() {
    let (ctx, tok, _) = setup(membership::Role::Conductor).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let (note, _) = create(&ctx, &tok, arr, "tempo", "first").await;
    let id: Uuid = note["id"].as_str().unwrap().parse().unwrap();
    let version = version_of(&ctx, arr, id).await;
    // updated_at comes from now(): make sure the first write lands in a later
    // millisecond, or the two versions would be equal by accident.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let first = lied::domain::global_annotation::update(&ctx.pool, id, version, None, Some("A"))
        .await
        .expect("the first writer wins");
    assert_eq!(first.content, "A");
    let second =
        lied::domain::global_annotation::update(&ctx.pool, id, version, None, Some("B")).await;
    assert!(matches!(
        second,
        Err(lied::domain::global_annotation::WriteError::Stale)
    ));
    let delete = lied::domain::global_annotation::delete(&ctx.pool, id, version).await;
    assert!(matches!(
        delete,
        Err(lied::domain::global_annotation::WriteError::Stale)
    ));
    let current = lied::domain::global_annotation::find(&ctx.pool, ctx.org, arr, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.content, "A", "B did not overwrite A");
}

/// Finding 3: a hidden annotation (its arrangement removed) cannot be deleted
/// or edited, even by a caller who read it a moment before, or restoring the
/// arrangement would not bring it back as it was.
#[tokio::test]
async fn a_hidden_annotation_cannot_be_deleted_or_edited() {
    let (ctx, tok, _) = setup(membership::Role::Conductor).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let (note, _) = create(&ctx, &tok, arr, "tempo", "keep me").await;
    let id: Uuid = note["id"].as_str().unwrap().parse().unwrap();
    let version = version_of(&ctx, arr, id).await;
    assert!(lied::domain::arrangement::soft_delete(&ctx.pool, arr)
        .await
        .unwrap());

    assert!(matches!(
        lied::domain::global_annotation::delete(&ctx.pool, id, version).await,
        Err(lied::domain::global_annotation::WriteError::NotFound)
    ));
    assert!(matches!(
        lied::domain::global_annotation::update(&ctx.pool, id, version, None, Some("x")).await,
        Err(lied::domain::global_annotation::WriteError::NotFound)
    ));
    assert!(lied::domain::arrangement::undelete(&ctx.pool, arr)
        .await
        .unwrap());
    let back = lied::domain::global_annotation::find(&ctx.pool, ctx.org, arr, id)
        .await
        .unwrap()
        .expect("restoring the arrangement brings it back");
    assert_eq!(back.content, "keep me");
}

/// Finding 4: a PATCH that changes nothing writes nothing — same ETag, no
/// audit row — and a misspelled field is refused, not a silent no-op.
#[tokio::test]
async fn a_patch_that_changes_nothing_writes_nothing() {
    let (ctx, tok, _) = setup(membership::Role::Conductor).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let (note, etag) = create(&ctx, &tok, arr, "tempo", "same").await;
    let id: Uuid = note["id"].as_str().unwrap().parse().unwrap();
    let url = format!("{}/{id}", list_url(ctx.org, arr));

    for body in [
        serde_json::json!({}),
        serde_json::json!({ "type": "tempo", "content": "same" }),
    ] {
        let (status, got, headers) = send(
            &ctx.app,
            req_with_if_match("PATCH", &url, &tok, Some(&etag), Some(body.clone())),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert_eq!(got["content"], "same");
        assert_eq!(
            headers.get("etag").unwrap().to_str().unwrap(),
            etag,
            "no new version for {body}"
        );
    }
    assert!(audit_rows(&ctx.pool, "global_annotation.update", id)
        .await
        .is_empty());

    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match(
            "PATCH",
            &url,
            &tok,
            Some(&etag),
            Some(serde_json::json!({ "contents": "typo" })),
        ),
    )
    .await;
    assert!(
        status.is_client_error() && status != axum::http::StatusCode::PRECONDITION_FAILED,
        "an unknown field is refused, got {status}"
    );
    // A no-op PATCH still validates what it was sent.
    let (status, _, _) = send(
        &ctx.app,
        req_with_if_match(
            "PATCH",
            &url,
            &tok,
            Some(&etag),
            Some(serde_json::json!({ "type": "cut" })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}
