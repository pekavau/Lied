//! Integration tests for issue #55: `Voice.part_number` (migration backfill +
//! `/v1` round trip) and bulk assignment of one player to the same part across
//! a collection (preview + apply). Drives the real `/v1` router with bearer
//! auth against a real Postgres; no MinIO, since none of this touches files.

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
        jwt_signing_key: Secret::from(
            "test-signing-key-material-for-bulk-assign-tests".to_string(),
        ),
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

async fn instrument(pool: &PgPool, key: &str) -> Uuid {
    sqlx::query_scalar!(r#"SELECT id FROM instrument WHERE key = $1"#, key)
        .fetch_one(pool)
        .await
        .expect("seeded instrument")
}

async fn voice(
    pool: &PgPool,
    arr_id: Uuid,
    name: &str,
    instrument_id: Uuid,
    part_number: Option<i16>,
) -> Uuid {
    let id = Uuid::now_v7();
    lied::domain::voice::create(
        pool,
        id,
        arr_id,
        name,
        &lied::domain::voice::slugify(name),
        instrument_id,
        lied::domain::voice::NewPartNumber::Set(part_number),
        None,
    )
    .await
    .expect("voice");
    id
}

async fn assign(pool: &PgPool, item: Uuid, voice: Uuid, user: Uuid) {
    lied::domain::part_assignment::assign(pool, Uuid::now_v7(), item, voice, user, None)
        .await
        .expect("assign");
}

async fn holds(pool: &PgPool, item: Uuid, voice: Uuid) -> Vec<Uuid> {
    lied::domain::part_assignment::list_for_item_voice(pool, item, voice)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.user_id)
        .collect()
}

async fn assignment_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar!(r#"SELECT count(*) as "n!" FROM part_assignment"#)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn preview_url(org: Uuid, coll: Uuid, user: Uuid, inst: Uuid, part: Option<i16>) -> String {
    let mut url = format!(
        "/v1/orgs/{org}/collections/{coll}/bulk-assignments/preview?userId={user}&instrumentId={inst}"
    );
    if let Some(n) = part {
        url.push_str(&format!("&partNumber={n}"));
    }
    url
}

fn bulk_url(org: Uuid, coll: Uuid) -> String {
    format!("/v1/orgs/{org}/collections/{coll}/bulk-assignments")
}

/// The preview entries' outcomes, in running order.
fn outcomes(body: &serde_json::Value) -> Vec<String> {
    body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["outcome"].as_str().unwrap().to_string())
        .collect()
}

/// The `assign` entries of a preview, as an apply request's `assignments`.
fn accepted(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["outcome"] == "assign")
        .map(|i| {
            serde_json::json!({
                "collectionItemId": i["collectionItemId"],
                "voiceId": i["voice"]["voiceId"],
            })
        })
        .collect()
}

/// A march book exercising every outcome for "1st trumpet":
///
/// 1. Trumpet 1 + Trumpet 2              → assign Trumpet 1
/// 2. "Trumpet" with no part number      → noMatch, candidate listed
/// 3. no trumpet at all (Flute 1)        → noMatch, no candidates
/// 4. two part-1 trumpets                → ambiguous
/// 5. arrangement removed                → removed
/// 6. player already plays Trumpet 1     → alreadyHeld
/// 7. player already plays the Flugelhorn → assign, with alsoHolds
struct MarchBook {
    coll: Uuid,
    items: Vec<Uuid>,
    trumpet: Uuid,
    /// The Trumpet 1 voice of piece 1, and piece 7.
    first_trumpet_1: Uuid,
    seventh_trumpet_1: Uuid,
    /// A second, unrelated player already on piece 1's Trumpet 1.
    other_player: Uuid,
}

async fn march_book(ctx: &Ctx, player: Uuid) -> MarchBook {
    let pool = &ctx.pool;
    let trumpet = instrument(pool, "trumpet_bb").await;
    let flute = instrument(pool, "flute").await;
    let flugel = instrument(pool, "flugelhorn").await;
    let coll = create_collection(pool, ctx.org, "march-book").await;

    let mut arrs = Vec::new();
    let mut items = Vec::new();
    for i in 1..=7 {
        let arr = create_arrangement(pool, ctx.org, &format!("march-{i}")).await;
        items.push(create_item(pool, coll, arr, i).await);
        arrs.push(arr);
    }

    let first_trumpet_1 = voice(pool, arrs[0], "Trumpet in Bb 1", trumpet, Some(1)).await;
    voice(pool, arrs[0], "Trumpet in Bb 2", trumpet, Some(2)).await;
    voice(pool, arrs[1], "Trumpet", trumpet, None).await;
    voice(pool, arrs[2], "Flute 1", flute, Some(1)).await;
    voice(pool, arrs[3], "Trumpet 1", trumpet, Some(1)).await;
    voice(pool, arrs[3], "Trumpet 1 (alt)", trumpet, Some(1)).await;
    voice(pool, arrs[4], "Trumpet 1", trumpet, Some(1)).await;
    sqlx::query!(
        "UPDATE arrangement SET deleted_at = now() WHERE id = $1",
        arrs[4]
    )
    .execute(pool)
    .await
    .unwrap();
    let sixth = voice(pool, arrs[5], "Trumpet 1", trumpet, Some(1)).await;
    assign(pool, items[5], sixth, player).await;
    let seventh_trumpet_1 = voice(pool, arrs[6], "Trumpet 1", trumpet, Some(1)).await;
    let seventh_flugel = voice(pool, arrs[6], "Flugelhorn", flugel, None).await;
    assign(pool, items[6], seventh_flugel, player).await;

    let other_player = create_user(pool, "other-trumpet").await;
    assign(pool, items[0], first_trumpet_1, other_player).await;

    MarchBook {
        coll,
        items,
        trumpet,
        first_trumpet_1,
        seventh_trumpet_1,
        other_player,
    }
}

// ── part_number ──────────────────────────────────────────────────────────────

/// Pins the backfill heuristic of migration 0024: confident answers only, and
/// NULL for anything that could be a combined or doubling part.
#[tokio::test]
async fn the_backfill_guess_is_conservative() {
    let db = TestDb::create_and_migrate().await;
    let cases: &[(&str, Option<i16>)] = &[
        ("Trumpet in Bb 1", Some(1)),
        ("Horn in F 12", Some(12)),
        ("Violin I", Some(1)),
        ("Violin II", Some(2)),
        (" Violin III ", Some(3)),
        ("Viola VIII", Some(8)),
        ("1st Trumpet", Some(1)),
        ("2nd Violin", Some(2)),
        ("2. Trompete", Some(2)),
        // No number at all.
        ("Timpani", None),
        ("Solo Cornet", None),
        // A key, not an ordinal.
        ("Clarinet in A", None),
        ("Horn in D", None),
        // Combined or doubling parts must not be guessed.
        ("Horn 1 & 3", None),
        ("Flute 2/Piccolo", None),
        ("Trombone 1+2", None),
        ("Trumpets 1, 2", None),
        ("2nd Violin 1", None),
        // Out of range.
        ("Part 0", None),
        ("Trumpet 100", None),
        // Combined parts written with roman numerals or words (review of #55:
        // the first version only rejected digits and / & + ,).
        ("Violin I und II", None),
        ("Horn I and III", None),
        ("Trompete I u. II", None),
        ("Violin I - II", None),
        ("1st Violin II", None),
        ("Trumpet 1 Solo", None),
        ("Violin ii", None),
    ];
    for (name, expected) in cases {
        let got: Option<i16> = sqlx::query_scalar("SELECT lied_guess_part_number($1)")
            .bind(name)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(got, *expected, "lied_guess_part_number({name:?})");
    }
}

/// POST a voice and return its JSON.
async fn post_voice(ctx: &Ctx, tok: &str, url: &str, body: serde_json::Value) -> serde_json::Value {
    let (status, created, _) = send(&ctx.app, req("POST", url, tok, Some(body))).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    created
}

/// PATCH a voice with a fresh If-Match and return its JSON.
async fn patch_voice(
    ctx: &Ctx,
    tok: &str,
    url: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let (_, _, headers) = send(&ctx.app, req("GET", url, tok, None)).await;
    let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();
    let (status, updated, _) = send(
        &ctx.app,
        req_with_if_match("PATCH", url, tok, Some(&etag), Some(body)),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{updated}");
    updated
}

#[tokio::test]
async fn a_voice_carries_a_part_number_over_v1() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let trumpet = instrument(&ctx.pool, "trumpet_bb").await;
    let url = format!("/v1/orgs/{}/arrangements/{arr}/voices", ctx.org);

    // Explicit.
    let created = post_voice(
        &ctx,
        &tok,
        &url,
        serde_json::json!({ "name": "Trumpet in Bb 2", "instrumentId": trumpet, "partNumber": 2 }),
    )
    .await;
    assert_eq!(created["partNumber"], 2);

    // Explicit null: a part with no number, whatever the name says.
    let unnumbered = post_voice(
        &ctx,
        &tok,
        &url,
        serde_json::json!({ "name": "Trumpet 4", "instrumentId": trumpet, "partNumber": null }),
    )
    .await;
    assert!(unnumbered["partNumber"].is_null(), "{unnumbered}");

    // Zero is not a part number.
    let (status, problem, _) = send(
        &ctx.app,
        req(
            "POST",
            &url,
            &tok,
            Some(serde_json::json!({
                "name": "Trumpet 0", "instrumentId": trumpet, "partNumber": 0
            })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(problem["errors"]["partNumber"].is_array(), "{problem}");

    // Update with an explicit number replaces it.
    let voice_url = format!("{url}/{}", created["id"].as_str().unwrap());
    let updated = patch_voice(
        &ctx,
        &tok,
        &voice_url,
        serde_json::json!({ "name": "Trumpet in Bb 3", "instrumentId": trumpet, "partNumber": 3 }),
    )
    .await;
    assert_eq!(updated["partNumber"], 3);
}

/// Review of #55, finding 7: voices created after the migration must number
/// like the backfilled ones, or every new piece silently drops out of bulk
/// assignment. Omitting `partNumber` on create guesses it from the name.
#[tokio::test]
async fn a_new_voice_without_a_part_number_takes_it_from_its_name() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let trumpet = instrument(&ctx.pool, "trumpet_bb").await;
    let url = format!("/v1/orgs/{}/arrangements/{arr}/voices", ctx.org);

    for (name, expected) in [
        ("Trumpet in Bb 2", serde_json::json!(2)),
        ("Violin II", serde_json::json!(2)),
        ("Trumpet", serde_json::Value::Null),
        ("Horn 1 & 3", serde_json::Value::Null),
    ] {
        let created = post_voice(
            &ctx,
            &tok,
            &url,
            serde_json::json!({ "name": name, "instrumentId": trumpet }),
        )
        .await;
        assert_eq!(created["partNumber"], expected, "{name}");
    }
}

/// Review of #55, finding 2: a PATCH that omits `partNumber` must keep it.
/// The body is otherwise replaced wholesale (#23), so a client that predates
/// the field would silently erase every backfilled number.
#[tokio::test]
async fn a_patch_that_omits_the_part_number_keeps_it_and_null_clears_it() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let arr = create_arrangement(&ctx.pool, ctx.org, "bolero").await;
    let trumpet = instrument(&ctx.pool, "trumpet_bb").await;
    let url = format!("/v1/orgs/{}/arrangements/{arr}/voices", ctx.org);
    let created = post_voice(
        &ctx,
        &tok,
        &url,
        serde_json::json!({ "name": "Trumpet in Bb 1", "instrumentId": trumpet, "partNumber": 1 }),
    )
    .await;
    let voice_url = format!("{url}/{}", created["id"].as_str().unwrap());

    // A rename by a client that knows nothing about part numbers.
    let renamed = patch_voice(
        &ctx,
        &tok,
        &voice_url,
        serde_json::json!({ "name": "Trumpet in B♭ 1", "instrumentId": trumpet }),
    )
    .await;
    assert_eq!(renamed["name"], "Trumpet in B♭ 1");
    assert_eq!(renamed["partNumber"], 1, "omitting the field keeps it");

    let cleared = patch_voice(
        &ctx,
        &tok,
        &voice_url,
        serde_json::json!({ "name": "Trumpet in B♭ 1", "instrumentId": trumpet, "partNumber": null }),
    )
    .await;
    assert!(cleared["partNumber"].is_null(), "explicit null clears it");
}

// ── preview ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_preview_reports_what_happens_to_every_piece() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;

    let (status, body, _) = send(
        &ctx.app,
        req(
            "GET",
            &preview_url(ctx.org, book.coll, player, book.trumpet, Some(1)),
            &tok,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(
        outcomes(&body),
        vec![
            "assign",
            "noMatch",
            "noMatch",
            "ambiguous",
            "removed",
            "alreadyHeld",
            "assign"
        ]
    );
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["voice"]["name"], "Trumpet in Bb 1");
    // A part without a number is shown, so the archivist can see why it did
    // not match and fix it.
    assert_eq!(items[1]["candidates"][0]["name"], "Trumpet");
    assert!(items[1]["candidates"][0]["partNumber"].is_null());
    assert_eq!(items[2]["candidates"].as_array().unwrap().len(), 0);
    assert_eq!(items[3]["candidates"].as_array().unwrap().len(), 2);
    // Holding another part of the same piece is reported, not refused.
    assert_eq!(items[6]["alsoHolds"][0]["name"], "Flugelhorn");
    assert_eq!(items[0]["alsoHolds"].as_array().unwrap().len(), 0);

    // The preview writes nothing.
    assert_eq!(assignment_rows(&ctx.pool).await, 3);
}

#[tokio::test]
async fn an_unnumbered_part_matches_only_unnumbered_voices() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "timpanist").await;
    let timpani = instrument(&ctx.pool, "timpani").await;
    let coll = create_collection(&ctx.pool, ctx.org, "program").await;
    let a = create_arrangement(&ctx.pool, ctx.org, "one").await;
    let b = create_arrangement(&ctx.pool, ctx.org, "two").await;
    create_item(&ctx.pool, coll, a, 1).await;
    create_item(&ctx.pool, coll, b, 2).await;
    voice(&ctx.pool, a, "Timpani", timpani, None).await;
    voice(&ctx.pool, b, "Timpani 1", timpani, Some(1)).await;

    let (_, body, _) = send(
        &ctx.app,
        req(
            "GET",
            &preview_url(ctx.org, coll, player, timpani, None),
            &tok,
            None,
        ),
    )
    .await;
    assert_eq!(outcomes(&body), vec!["assign", "noMatch"]);
}

#[tokio::test]
async fn a_preview_naming_nothing_real_is_refused() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;

    for (url, field) in [
        (
            preview_url(ctx.org, book.coll, Uuid::now_v7(), book.trumpet, Some(1)),
            "userId",
        ),
        (
            preview_url(ctx.org, book.coll, player, Uuid::now_v7(), Some(1)),
            "instrumentId",
        ),
        // Zero is not a part number; it must not read as "no piece has it".
        (
            preview_url(ctx.org, book.coll, player, book.trumpet, Some(0)),
            "partNumber",
        ),
    ] {
        let (status, problem, _) = send(&ctx.app, req("GET", &url, &tok, None)).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{url}");
        assert!(problem["errors"][field].is_array(), "{problem}");
    }
}

// ── apply ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn applying_assigns_exactly_the_accepted_pieces_and_adds_rather_than_replaces() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;
    let (_, preview, _) = send(
        &ctx.app,
        req(
            "GET",
            &preview_url(ctx.org, book.coll, player, book.trumpet, Some(1)),
            &tok,
            None,
        ),
    )
    .await;
    let pairs = accepted(&preview);
    assert_eq!(pairs.len(), 2);

    let (status, body, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(ctx.org, book.coll),
            &tok,
            Some(serde_json::json!({ "userId": player, "instrumentId": book.trumpet, "partNumber": 1, "assignments": pairs })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["created"].as_array().unwrap().len(), 2);
    assert_eq!(body["alreadyHeld"].as_array().unwrap().len(), 0);
    assert_eq!(assignment_rows(&ctx.pool).await, 5);

    // Piece 1 now has BOTH trumpeters: bulk assignment adds a player.
    let mut on_first = holds(&ctx.pool, book.items[0], book.first_trumpet_1).await;
    on_first.sort();
    let mut expected = vec![player, book.other_player];
    expected.sort();
    assert_eq!(on_first, expected);
    assert_eq!(
        holds(&ctx.pool, book.items[6], book.seventh_trumpet_1).await,
        vec![player]
    );

    // Re-sending is harmless: everything is reported as already held.
    let (status, again, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(ctx.org, book.coll),
            &tok,
            Some(serde_json::json!({ "userId": player, "instrumentId": book.trumpet, "partNumber": 1, "assignments": accepted(&preview) })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(again["created"].as_array().unwrap().len(), 0);
    assert_eq!(again["alreadyHeld"].as_array().unwrap().len(), 2);
    assert_eq!(assignment_rows(&ctx.pool).await, 5);

    // One ordinary audit row per created assignment, marked as bulk.
    let bulk_rows: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "n!" FROM audit_log
           WHERE action = 'part_assignment.create' AND payload->>'via' = 'bulk'"#
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(bulk_rows, 2);
}

#[tokio::test]
async fn a_plan_that_went_stale_is_refused_and_writes_nothing() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;
    let (_, preview, _) = send(
        &ctx.app,
        req(
            "GET",
            &preview_url(ctx.org, book.coll, player, book.trumpet, Some(1)),
            &tok,
            None,
        ),
    )
    .await;
    let pairs = accepted(&preview);
    let before = assignment_rows(&ctx.pool).await;

    // Between preview and confirm, piece 7's trumpet part is removed.
    sqlx::query!(
        "UPDATE voice SET deleted_at = now() WHERE id = $1",
        book.seventh_trumpet_1
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    let (status, problem, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(ctx.org, book.coll),
            &tok,
            Some(serde_json::json!({ "userId": player, "instrumentId": book.trumpet, "partNumber": 1, "assignments": pairs })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{problem}");
    assert_eq!(
        assignment_rows(&ctx.pool).await,
        before,
        "all or nothing: piece 1 must not have been assigned either"
    );

    // A pair from a different collection is stale in the same way.
    let other = create_collection(&ctx.pool, ctx.org, "other").await;
    let (status, _, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(ctx.org, other),
            &tok,
            Some(serde_json::json!({
                "userId": player, "instrumentId": book.trumpet, "partNumber": 1,
                "assignments": [{
                    "collectionItemId": book.items[0], "voiceId": book.first_trumpet_1
                }],
            })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(assignment_rows(&ctx.pool).await, before);
}

/// The voice id of `name` in the piece at `index` of the book.
async fn voice_named(pool: &PgPool, item: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar!(
        r#"SELECT v.id FROM voice v
           JOIN collection_item ci ON ci.arrangement_id = v.arrangement_id
           WHERE ci.id = $1 AND v.name = $2"#,
        item,
        name,
    )
    .fetch_one(pool)
    .await
    .expect("voice by name")
}

/// Review of #55, finding 3: the confirm must refuse a voice that is still
/// live but is no longer the requested part. Renumbering is exactly what the
/// preview tells the archivist to go and do, so a confirm left open in another
/// tab must not put the 1st trumpet on what is now Trumpet 2.
#[tokio::test]
async fn a_voice_renumbered_or_moved_since_the_preview_makes_the_confirm_stale() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;
    let flute = instrument(&ctx.pool, "flute").await;
    let (_, preview, _) = send(
        &ctx.app,
        req(
            "GET",
            &preview_url(ctx.org, book.coll, player, book.trumpet, Some(1)),
            &tok,
            None,
        ),
    )
    .await;
    let pairs = accepted(&preview);
    let before = assignment_rows(&ctx.pool).await;
    let apply = |pairs: Vec<serde_json::Value>| {
        req(
            "POST",
            &bulk_url(ctx.org, book.coll),
            &tok,
            Some(serde_json::json!({
                "userId": player, "instrumentId": book.trumpet, "partNumber": 1,
                "assignments": pairs,
            })),
        )
    };

    // Renumbered: piece 7's Trumpet 1 is now part 2.
    sqlx::query!(
        "UPDATE voice SET part_number = 2 WHERE id = $1",
        book.seventh_trumpet_1
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    let (status, _, _) = send(&ctx.app, apply(pairs.clone())).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(assignment_rows(&ctx.pool).await, before, "all or nothing");

    // Moved to another instrument: piece 1's Trumpet 1 is now a flute part.
    sqlx::query!(
        "UPDATE voice SET part_number = 1 WHERE id = $1",
        book.seventh_trumpet_1
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE voice SET instrument_id = $2 WHERE id = $1",
        book.first_trumpet_1,
        flute
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    let (status, _, _) = send(&ctx.app, apply(pairs.clone())).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(assignment_rows(&ctx.pool).await, before);

    // Put back, the same confirm goes through: the 409s were the changes.
    sqlx::query!(
        "UPDATE voice SET instrument_id = $2 WHERE id = $1",
        book.first_trumpet_1,
        book.trumpet
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    let (status, _, _) = send(&ctx.app, apply(pairs)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

/// Review of #55, finding 4: a piece the matcher cannot settle (ambiguous, or
/// no exact match) that was settled by hand on its Parts screen must read as
/// done, or re-running the preview never converges.
#[tokio::test]
async fn a_piece_settled_by_hand_reads_as_already_held() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;
    // Piece 2 has only an unnumbered "Trumpet"; piece 4 has two Trumpet 1s.
    let unnumbered = voice_named(&ctx.pool, book.items[1], "Trumpet").await;
    let alt = voice_named(&ctx.pool, book.items[3], "Trumpet 1 (alt)").await;
    assign(&ctx.pool, book.items[1], unnumbered, player).await;
    assign(&ctx.pool, book.items[3], alt, player).await;

    let (_, body, _) = send(
        &ctx.app,
        req(
            "GET",
            &preview_url(ctx.org, book.coll, player, book.trumpet, Some(1)),
            &tok,
            None,
        ),
    )
    .await;
    let outcomes = outcomes(&body);
    assert_eq!(outcomes[1], "alreadyHeld", "{body}");
    assert_eq!(outcomes[3], "alreadyHeld", "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[1]["voice"]["name"], "Trumpet", "says which voice");
    assert_eq!(items[3]["voice"]["name"], "Trumpet 1 (alt)");

    // Someone ELSE holding a candidate does not settle it for this player.
    let other = create_user(&ctx.pool, "second-trumpet").await;
    let (_, body, _) = send(
        &ctx.app,
        req(
            "GET",
            &preview_url(ctx.org, book.coll, other, book.trumpet, Some(1)),
            &tok,
            None,
        ),
    )
    .await;
    let outcomes = self::outcomes(&body);
    assert_eq!(outcomes[1], "noMatch");
    assert_eq!(outcomes[3], "ambiguous");
}

/// Review of #55, finding 8: the audit rows are written in the apply's own
/// transaction, one per created assignment, carrying the request's context —
/// not fanned out afterwards where a dropped request loses some of them.
#[tokio::test]
async fn bulk_audit_rows_are_written_with_the_assignments() {
    let (ctx, _, actor) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;
    let request_id = Uuid::now_v7();
    let audit_ctx = lied::domain::audit_log::AuditContext {
        actor_user_id: Some(actor),
        org_id: Some(ctx.org),
        request_id: Some(request_id),
    };
    let key = lied::domain::bulk_assign::PartKey {
        instrument_id: book.trumpet,
        part_number: Some(1),
    };
    let applied = lied::domain::bulk_assign::apply(
        &ctx.pool,
        book.coll,
        key,
        player,
        &[
            (book.items[0], book.first_trumpet_1),
            (book.items[6], book.seventh_trumpet_1),
        ],
        &audit_ctx,
    )
    .await
    .expect("apply");
    assert_eq!(applied.created.len(), 2);

    let mut audited: Vec<Uuid> = sqlx::query_scalar!(
        r#"SELECT target_id as "target_id!" FROM audit_log
           WHERE action = 'part_assignment.create' AND request_id = $1
             AND actor_user_id = $2 AND org_id = $3 AND payload->>'via' = 'bulk'"#,
        request_id,
        actor,
        ctx.org,
    )
    .fetch_all(&ctx.pool)
    .await
    .unwrap();
    audited.sort();
    let mut created: Vec<Uuid> = applied.created.iter().map(|a| a.id).collect();
    created.sort();
    assert_eq!(audited, created, "exactly one audit row per assignment");

    // A refused (stale) apply writes neither assignments nor audit rows.
    let stale_request = Uuid::now_v7();
    let result = lied::domain::bulk_assign::apply(
        &ctx.pool,
        book.coll,
        lied::domain::bulk_assign::PartKey {
            instrument_id: book.trumpet,
            part_number: Some(2),
        },
        player,
        &[(book.items[0], book.first_trumpet_1)],
        &lied::domain::audit_log::AuditContext {
            request_id: Some(stale_request),
            ..audit_ctx
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(lied::domain::bulk_assign::BulkAssignError::Stale)
    ));
    let none: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "n!" FROM audit_log WHERE request_id = $1"#,
        stale_request
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(none, 0);
}

#[tokio::test]
async fn an_empty_or_unknown_apply_is_a_400() {
    let (ctx, tok, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;

    let (status, problem, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(ctx.org, book.coll),
            &tok,
            Some(serde_json::json!({ "userId": player, "instrumentId": book.trumpet, "partNumber": 1, "assignments": [] })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(problem["errors"]["assignments"].is_array(), "{problem}");

    let (status, problem, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(ctx.org, book.coll),
            &tok,
            Some(serde_json::json!({
                "userId": Uuid::now_v7(), "instrumentId": book.trumpet, "partNumber": 1,
                "assignments": [{
                    "collectionItemId": book.items[0], "voiceId": book.first_trumpet_1
                }],
            })),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(problem["errors"]["userId"].is_array(), "{problem}");
}

// ── authorization ───────────────────────────────────────────────────────────

#[tokio::test]
async fn only_part_assigners_may_preview_or_apply_and_other_orgs_see_404() {
    let (ctx, _, _) = setup(membership::Role::Archivist).await;
    let player = create_user(&ctx.pool, "first-trumpet").await;
    let book = march_book(&ctx, player).await;
    let preview = preview_url(ctx.org, book.coll, player, book.trumpet, Some(1));
    let apply_body = serde_json::json!({
        "userId": player, "instrumentId": book.trumpet, "partNumber": 1,
        "assignments": [{ "collectionItemId": book.items[0], "voiceId": book.first_trumpet_1 }],
    });

    // A plain musician may do neither — the preview reveals what a named user
    // holds across the collection.
    let musician = create_user(&ctx.pool, "musician").await;
    add_member(&ctx.pool, ctx.org, musician, membership::Role::Musician).await;
    let mtok = ctx.token_for(musician);
    let (status, _, _) = send(&ctx.app, req("GET", &preview, &mtok, None)).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let (status, _, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(ctx.org, book.coll),
            &mtok,
            Some(apply_body.clone()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(
        holds(&ctx.pool, book.items[0], book.first_trumpet_1)
            .await
            .len(),
        1
    );

    // A conductor may (permission matrix: part assignments).
    let conductor = create_user(&ctx.pool, "conductor").await;
    add_member(&ctx.pool, ctx.org, conductor, membership::Role::Conductor).await;
    let ctok = ctx.token_for(conductor);
    let (status, _, _) = send(&ctx.app, req("GET", &preview, &ctok, None)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let (status, _, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(ctx.org, book.coll),
            &ctok,
            Some(apply_body.clone()),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // An archivist of another org addressing this collection through their own
    // org's path gets 404, not someone else's march book.
    let other_org = create_org(&ctx.pool, "Rival").await;
    let outsider = create_user(&ctx.pool, "outsider").await;
    add_member(&ctx.pool, other_org, outsider, membership::Role::Archivist).await;
    let otok = ctx.token_for(outsider);
    let (status, _, _) = send(
        &ctx.app,
        req(
            "GET",
            &preview_url(other_org, book.coll, player, book.trumpet, Some(1)),
            &otok,
            None,
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    let (status, _, _) = send(
        &ctx.app,
        req(
            "POST",
            &bulk_url(other_org, book.coll),
            &otok,
            Some(apply_body),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}
