//! Integration tests for issue #35 (coverage check, UC-13) through `/v1`.
//!
//! The interesting half is authorization: coverage is the one non-staff-facing
//! view in this tree, so "a principal sees exactly their section and nothing
//! else" is the assertion that matters most, and the one whose failure would be
//! invisible from the outside.
//!
//! Harness copied from `tests/search.rs` (the established pattern).

use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{
    arrangement, collection, collection_item, coverage, membership, organization, part_assignment,
    user, voice,
};
use lied::state::AppState;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

struct TestDb {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDb {
    async fn create_and_migrate() -> Self {
        let container = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("failed to start postgres container");

        let host = container.get_host().await.expect("container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("container port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("failed to connect to container postgres");

        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        Self {
            _container: container,
            pool,
        }
    }
}

fn test_config() -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from(
            "test-signing-key-material-for-arrangement-tests".to_string(),
        ),
        jwt_lifetime_days: 30,
        max_upload_bytes: 1,
        max_request_bytes: 256 * 1024,
        max_files_per_voice: 1,
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

async fn build_test_app(pool: PgPool) -> (axum::Router, AppState) {
    let config = test_config();
    let state = test_state(pool, config);
    (lied::routes::build_router(state.clone()), state)
}

async fn create_plain_user(pool: &PgPool, username: &str) -> user::User {
    let hash = auth::password::hash_password("correct horse battery staple").expect("hash");
    let id = Uuid::now_v7();
    let slug = user::slugify(username);
    user::create(
        pool,
        id,
        &slug,
        username,
        None,
        Some(&hash),
        username,
        false,
        None,
    )
    .await
    .expect("create user")
}

async fn create_org(pool: &PgPool, name: &str) -> organization::Organization {
    let id = Uuid::now_v7();
    let slug = organization::slugify(name);
    organization::create(pool, id, name, &slug, None)
        .await
        .expect("create org")
}

async fn add_member(pool: &PgPool, org_id: Uuid, user_id: Uuid, role: membership::Role) {
    let id = Uuid::now_v7();
    membership::create(
        pool,
        id,
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
    .expect("create membership");
}

/// A user holding `role` in a freshly created org. Returns `(org_id, token)`.
async fn org_with_member(
    pool: &PgPool,
    state: &AppState,
    org_name: &str,
    username: &str,
    role: membership::Role,
) -> (Uuid, String) {
    let org = create_org(pool, org_name).await;
    let u = create_plain_user(pool, username).await;
    add_member(pool, org.id, u.id, role).await;
    (org.id, mint_token(state, u.id))
}

fn mint_token(state: &AppState, user_id: Uuid) -> String {
    let (token, _claims) = state.jwt_keyring.mint(user_id, None).expect("mint token");
    token
}

fn bearer_request(method: &str, uri: &str, token: &str) -> axum::http::request::Builder {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

// ---------------------------------------------------------------------------
// Coverage fixtures
// ---------------------------------------------------------------------------

/// A programme with two pieces, each with two voices on distinct instruments.
/// Returns (collection, items, voices-by-instrument).
struct Programme {
    collection_id: Uuid,
    items: Vec<Uuid>,
    /// (voice_id, instrument_id) in creation order, two per item.
    voices: Vec<(Uuid, Uuid)>,
}

async fn two_instruments(pool: &PgPool) -> (Uuid, Uuid) {
    let ids: Vec<Uuid> = sqlx::query_scalar!(r#"SELECT id FROM instrument ORDER BY key LIMIT 2"#)
        .fetch_all(pool)
        .await
        .expect("instrument seed");
    (ids[0], ids[1])
}

async fn seed_programme(pool: &PgPool, org_id: Uuid) -> Programme {
    let (flute, trumpet) = two_instruments(pool).await;
    let collection_id = Uuid::now_v7();
    collection::create(
        pool,
        collection_id,
        org_id,
        "Spring Concert",
        "spring-concert",
        "program",
        None,
    )
    .await
    .expect("collection");

    let mut items = Vec::new();
    let mut voices = Vec::new();
    for (index, title) in [(1, "Bolero"), (2, "Egmont")] {
        let arr_id = Uuid::now_v7();
        arrangement::create(
            pool,
            arr_id,
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

        for (name, instrument) in [("Flute 1", flute), ("Trumpet 1", trumpet)] {
            let voice_id = Uuid::now_v7();
            voice::create(
                pool,
                voice_id,
                arr_id,
                name,
                &voice::slugify(&format!("{title}-{name}")),
                instrument,
                None,
            )
            .await
            .expect("voice");
            voices.push((voice_id, instrument));
        }

        let item_id = Uuid::now_v7();
        collection_item::create(pool, item_id, collection_id, arr_id, index, None)
            .await
            .expect("item");
        items.push(item_id);
    }

    Programme {
        collection_id,
        items,
        voices,
    }
}

async fn assign(pool: &PgPool, item: Uuid, voice: Uuid, user: Uuid) -> Uuid {
    let (row, _) = part_assignment::assign(pool, Uuid::now_v7(), item, voice, user, None)
        .await
        .expect("assign");
    row.id
}

async fn acknowledge(pool: &PgPool, assignment: Uuid) {
    part_assignment::update_state(pool, assignment, None, Some(chrono::Utc::now()))
        .await
        .expect("acknowledge");
}

async fn coverage_json(
    app: &axum::Router,
    token: &str,
    org_id: Uuid,
    collection_id: Uuid,
) -> serde_json::Value {
    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/collections/{collection_id}/coverage"),
        token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    body_json(response).await
}

async fn coverage_status(
    app: &axum::Router,
    token: &str,
    org_id: Uuid,
    collection_id: Uuid,
) -> u16 {
    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/collections/{collection_id}/coverage"),
        token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    app.clone()
        .oneshot(request)
        .await
        .unwrap()
        .status()
        .as_u16()
}

// ---------------------------------------------------------------------------
// The report itself
// ---------------------------------------------------------------------------

#[tokio::test]
async fn coverage_reports_gaps_assignments_and_rehearsal_per_piece() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Coverage Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let player = create_plain_user(&db.pool, "player").await;
    let programme = seed_programme(&db.pool, org_id).await;

    // Four required voices: leave one unassigned, assign two (one acknowledged).
    let a1 = assign(
        &db.pool,
        programme.items[0],
        programme.voices[0].0,
        player.id,
    )
    .await;
    assign(
        &db.pool,
        programme.items[0],
        programme.voices[1].0,
        player.id,
    )
    .await;
    assign(
        &db.pool,
        programme.items[1],
        programme.voices[2].0,
        player.id,
    )
    .await;
    acknowledge(&db.pool, a1).await;

    let report = coverage_json(&app, &token, org_id, programme.collection_id).await;
    assert_eq!(report["required"], 4);
    assert_eq!(
        report["assigned"], 3,
        "rehearsed voices count as assigned too"
    );
    assert_eq!(report["rehearsed"], 1);

    let items = report["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["required"], 2);
    assert_eq!(items[0]["assigned"], 2);
    assert_eq!(items[0]["rehearsed"], 1);
    assert_eq!(items[1]["assigned"], 1, "the second piece has a gap");

    // The three states are distinguishable per voice, which is what the
    // dashboard renders.
    let states: Vec<&str> = items
        .iter()
        .flat_map(|item| item["voices"].as_array().unwrap())
        .map(|v| v["state"].as_str().unwrap())
        .collect();
    assert_eq!(
        states,
        vec!["rehearsed", "assigned", "assigned", "unassigned"]
    );
}

#[tokio::test]
async fn soft_deleted_voices_items_and_arrangements_are_never_required() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Coverage Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let programme = seed_programme(&db.pool, org_id).await;
    assert_eq!(
        coverage_json(&app, &token, org_id, programme.collection_id).await["required"],
        4
    );

    // A retired voice stops being a gap to chase.
    voice::soft_delete(&db.pool, programme.voices[0].0)
        .await
        .unwrap();
    assert_eq!(
        coverage_json(&app, &token, org_id, programme.collection_id).await["required"],
        3
    );

    // An arrangement pulled from the catalogue keeps its slot in the programme
    // but requires nothing — a broken slot to see, not a gap to fill.
    let arrangement_id = Uuid::parse_str(
        coverage_json(&app, &token, org_id, programme.collection_id).await["items"][1]
            ["arrangementId"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    arrangement::soft_delete(&db.pool, arrangement_id)
        .await
        .unwrap();
    let report = coverage_json(&app, &token, org_id, programme.collection_id).await;
    assert_eq!(report["required"], 1, "only the first piece still counts");
    let removed = &report["items"][1];
    assert_eq!(removed["arrangementRemoved"], true);
    assert_eq!(removed["required"], 0);
    assert!(
        removed["voices"].as_array().unwrap().is_empty(),
        "a removed piece contributes no voices"
    );

    // A piece removed from the programme disappears entirely.
    collection_item::remove(&db.pool, programme.collection_id, programme.items[1])
        .await
        .unwrap();
    let report = coverage_json(&app, &token, org_id, programme.collection_id).await;
    assert_eq!(report["items"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Authorization — the carve-out
// ---------------------------------------------------------------------------

/// A musician in `org_id`, optionally a principal for `instruments`.
async fn seed_musician(
    pool: &PgPool,
    state: &AppState,
    org_id: Uuid,
    username: &str,
    is_principal: bool,
    instruments: &[Uuid],
) -> String {
    let user = create_plain_user(pool, username).await;
    membership::create(
        pool,
        Uuid::now_v7(),
        user.id,
        org_id,
        membership::MembershipFields {
            role: membership::Role::Musician,
            instrument_ids: instruments,
            is_principal,
            principal_instrument_ids: instruments,
        },
        None,
    )
    .await
    .expect("membership");
    mint_token(state, user.id)
}

#[tokio::test]
async fn a_principal_sees_their_section_and_nothing_else() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, staff_token) = org_with_member(
        &db.pool,
        &state,
        "Coverage Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let programme = seed_programme(&db.pool, org_id).await;
    let (flute, _trumpet) = two_instruments(&db.pool).await;
    let principal_token =
        seed_musician(&db.pool, &state, org_id, "flute-lead", true, &[flute]).await;

    // Staff see all four voices…
    let staff = coverage_json(&app, &staff_token, org_id, programme.collection_id).await;
    assert_eq!(staff["required"], 4);

    // …the flute principal sees only the two flute voices, across both pieces.
    let section = coverage_json(&app, &principal_token, org_id, programme.collection_id).await;
    assert_eq!(section["required"], 2, "only this principal's instrument");
    let instruments: Vec<&str> = section["items"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["voices"].as_array().unwrap())
        .map(|v| v["instrumentId"].as_str().unwrap())
        .collect();
    assert!(
        instruments.iter().all(|id| *id == flute.to_string()),
        "a principal must not see another section's voices: {instruments:?}"
    );
}

#[tokio::test]
async fn a_plain_musician_is_refused_and_an_unconfigured_principal_gets_an_empty_report() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, _staff) = org_with_member(
        &db.pool,
        &state,
        "Coverage Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let programme = seed_programme(&db.pool, org_id).await;

    // A musician who leads nothing has no business reading the programme's
    // coverage.
    let plain = seed_musician(&db.pool, &state, org_id, "rank-and-file", false, &[]).await;
    assert_eq!(
        coverage_status(&app, &plain, org_id, programme.collection_id).await,
        403
    );

    // A principal with no instruments configured gets an honest empty report,
    // not a 403 that would send them to complain to the wrong person.
    let unconfigured = seed_musician(&db.pool, &state, org_id, "new-lead", true, &[]).await;
    let report = coverage_json(&app, &unconfigured, org_id, programme.collection_id).await;
    assert_eq!(report["required"], 0);
    assert_eq!(report["assigned"], 0);
    assert!(
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["voices"].as_array().unwrap().is_empty()),
        "the pieces are listed, but nothing in them is this principal's"
    );
}

#[tokio::test]
async fn coverage_for_another_orgs_collection_is_not_found() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Coverage Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let (other_org, _other_token) = org_with_member(
        &db.pool,
        &state,
        "Rival Orchestra",
        "their-arch",
        membership::Role::Archivist,
    )
    .await;
    let theirs = seed_programme(&db.pool, other_org).await;

    // A coverage report names pieces and the people playing them, so a
    // cross-org id must 404 rather than leak another orchestra's programme.
    assert_eq!(
        coverage_status(&app, &token, org_id, theirs.collection_id).await,
        404
    );
}

#[tokio::test]
async fn the_org_wide_query_agrees_with_the_per_collection_one() {
    // The dashboard reads every collection in one query rather than looping the
    // per-collection one (an N+1 over a five-way join). Two queries answering
    // the same question is a drift risk, so they are checked against each other
    // over a corpus with each interesting shape present: covered, gapped, empty,
    // and a piece whose arrangement was removed.
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, _token) = org_with_member(
        &db.pool,
        &state,
        "Coverage Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let _ = &app;
    let player = create_plain_user(&db.pool, "player").await;

    let covered = seed_programme(&db.pool, org_id).await;
    let a1 = assign(&db.pool, covered.items[0], covered.voices[0].0, player.id).await;
    acknowledge(&db.pool, a1).await;

    // An empty collection, which the per-collection query would report as zeros
    // and the org-wide one must not drop.
    let empty = Uuid::now_v7();
    collection::create(
        &db.pool,
        empty,
        org_id,
        "Empty Book",
        "empty-book",
        "standing",
        None,
    )
    .await
    .expect("collection");

    for required in [
        coverage::Required::EveryVoice,
        coverage::Required::Instruments(vec![covered.voices[0].1]),
    ] {
        let org_wide = coverage::for_org(&db.pool, org_id, &required)
            .await
            .expect("org-wide coverage");
        assert!(
            org_wide.iter().any(|r| r.collection_id == empty),
            "an empty collection must still appear"
        );

        for report in &org_wide {
            let single = coverage::for_collection(&db.pool, report.collection_id, &required)
                .await
                .expect("per-collection coverage");
            assert_eq!(
                (report.required, report.assigned, report.rehearsed),
                (single.required, single.assigned, single.rehearsed),
                "the two queries disagree about collection {}",
                report.collection_id
            );
            assert_eq!(
                report.items.len(),
                single.items.len(),
                "…or about how many pieces it has"
            );
        }
    }
}
