//! Integration tests for issue #34 (archive search): the trigger-maintained
//! `tsvector`, trigram fuzzy matching, the faceted filters, and the relevance
//! sort — driven through `/v1` so the tests exercise the same path the console
//! search screen uses.
//!
//! The interesting cases are the ones a unit test cannot reach: that the vector
//! follows a Work's composer or a Tag's name *after* the arrangements were
//! indexed, which is what the trigger set exists for.
//!
//! Harness copied from `tests/arrangements.rs` (the established pattern).

use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{arrangement, membership, organization, tag, user, voice, work};
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

async fn first_instrument_id(pool: &PgPool) -> Uuid {
    sqlx::query_scalar!(r#"SELECT id FROM instrument ORDER BY key LIMIT 1"#)
        .fetch_one(pool)
        .await
        .expect("instrument seed present")
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
// Search-specific helpers
// ---------------------------------------------------------------------------

/// Titles returned by a search, in result order.
async fn search_titles(app: &axum::Router, token: &str, org_id: Uuid, query: &str) -> Vec<String> {
    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/arrangements?{query}"),
        token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "search '{query}' should succeed"
    );
    let body = body_json(response).await;
    body["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|item| item["title"].as_str().unwrap().to_string())
        .collect()
}

async fn search_status(app: &axum::Router, token: &str, org_id: Uuid, query: &str) -> u16 {
    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/arrangements?{query}"),
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

/// Create an arrangement with the fields search actually reads.
#[allow(clippy::too_many_arguments)]
async fn seed_searchable(
    pool: &PgPool,
    org_id: Uuid,
    title: &str,
    work_id: Option<Uuid>,
    arranger: Option<&str>,
    instrumentation: Option<&str>,
    difficulty: Option<i16>,
    duration_seconds: Option<i32>,
) -> Uuid {
    let id = Uuid::now_v7();
    arrangement::create(
        pool,
        id,
        org_id,
        &arrangement::slugify(title),
        arrangement::ArrangementFields {
            title,
            work_id,
            instrumentation,
            arranger,
            publisher: None,
            purchase_date: None,
            license_notes: None,
            copy_count_allowed: None,
            status: "active",
            duration_seconds,
            difficulty,
            difficulty_ratings: None,
            difficulty_notes: None,
        },
        None,
    )
    .await
    .expect("create arrangement");
    id
}

async fn seed_work(pool: &PgPool, title: &str, composer: &str, creator: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    work::create(pool, id, title, Some(composer), Some(creator))
        .await
        .expect("create work");
    id
}

// ---------------------------------------------------------------------------
// Full-text: what the vector covers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_text_finds_an_arrangement_by_composer_arranger_tag_and_instrumentation() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let creator = create_plain_user(&db.pool, "curator").await;

    let work = seed_work(&db.pool, "Boléro", "Maurice Ravel", creator.id).await;
    let bolero = seed_searchable(
        &db.pool,
        org_id,
        "Boléro",
        Some(work),
        Some("Hans Zimmermann"),
        Some("large orchestra with alto saxophone"),
        Some(5),
        Some(900),
    )
    .await;
    seed_searchable(
        &db.pool,
        org_id,
        "Unrelated Waltz",
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    // Composer lives on the Work, one join away — the phase-1 ILIKE reached it,
    // but only as a raw substring.
    assert_eq!(
        search_titles(&app, &token, org_id, "q=ravel").await,
        vec!["Boléro"]
    );
    // Arranger, instrumentation: new reach the ILIKE never had.
    assert_eq!(
        search_titles(&app, &token, org_id, "q=zimmermann").await,
        vec!["Boléro"]
    );
    assert_eq!(
        search_titles(&app, &token, org_id, "q=saxophone").await,
        vec!["Boléro"]
    );

    // Tag names, via the join table.
    let tag_id = Uuid::now_v7();
    tag::create(&db.pool, tag_id, org_id, "Spanish", Some("theme"), None)
        .await
        .expect("tag");
    tag::attach(&db.pool, Uuid::now_v7(), bolero, tag_id)
        .await
        .expect("attach");
    assert_eq!(
        search_titles(&app, &token, org_id, "q=spanish").await,
        vec!["Boléro"],
        "a tag attached after indexing must be searchable"
    );
}

#[tokio::test]
async fn accents_fold_in_both_directions_and_misspellings_still_match() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let creator = create_plain_user(&db.pool, "curator").await;
    let work = seed_work(&db.pool, "Slavonic Dances", "Antonín Dvořák", creator.id).await;
    seed_searchable(&db.pool, org_id, "Boléro", None, None, None, None, None).await;
    seed_searchable(
        &db.pool,
        org_id,
        "Slavonic Dances",
        Some(work),
        None,
        None,
        None,
        None,
    )
    .await;

    // Typing without the accents finds the accented text…
    assert_eq!(
        search_titles(&app, &token, org_id, "q=bolero").await,
        vec!["Boléro"]
    );
    assert_eq!(
        search_titles(&app, &token, org_id, "q=dvorak").await,
        vec!["Slavonic Dances"]
    );
    // …and typing them finds it too.
    assert_eq!(
        search_titles(&app, &token, org_id, "q=Bol%C3%A9ro").await,
        vec!["Boléro"]
    );
    // A misspelling the tokeniser cannot match falls to trigram similarity.
    assert_eq!(
        search_titles(&app, &token, org_id, "q=bolerro").await,
        vec!["Boléro"],
        "trigram similarity is what makes a half-remembered title findable"
    );
}

#[tokio::test]
async fn a_soft_deleted_arrangement_never_matches() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let id = seed_searchable(&db.pool, org_id, "Boléro", None, None, None, None, None).await;
    assert_eq!(
        search_titles(&app, &token, org_id, "q=bolero").await.len(),
        1
    );

    arrangement::soft_delete(&db.pool, id).await.unwrap();
    assert!(
        search_titles(&app, &token, org_id, "q=bolero")
            .await
            .is_empty(),
        "search must not surface soft-deleted rows"
    );
}

// ---------------------------------------------------------------------------
// The cross-table trigger paths — the reason the vector is trigger-maintained
// ---------------------------------------------------------------------------

#[tokio::test]
async fn renaming_a_works_composer_reindexes_its_arrangements() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let creator = create_plain_user(&db.pool, "curator").await;
    let work = seed_work(&db.pool, "Symphony No. 9", "Antonin Dvorak", creator.id).await;
    seed_searchable(
        &db.pool,
        org_id,
        "From the New World",
        Some(work),
        None,
        None,
        None,
        None,
    )
    .await;
    assert_eq!(
        search_titles(&app, &token, org_id, "q=dvorak").await,
        vec!["From the New World"]
    );

    // Fix the composer's spelling *after* the arrangement was indexed: the
    // vector lives on `arrangement`, so nothing but a trigger keeps it true.
    work::update(&db.pool, work, "Symphony No. 9", Some("Antonín Dvořák"))
        .await
        .expect("update work");

    assert_eq!(
        search_titles(&app, &token, org_id, "q=dvorak").await,
        vec!["From the New World"],
        "the corrected spelling is still reachable unaccented"
    );
    assert_eq!(
        search_titles(&app, &token, org_id, "q=Dvo%C5%99%C3%A1k").await,
        vec!["From the New World"],
        "…and by the new exact spelling, which means the vector was rebuilt"
    );
}

#[tokio::test]
async fn renaming_or_detaching_a_tag_reindexes_its_arrangements() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let id = seed_searchable(&db.pool, org_id, "Fanfare", None, None, None, None, None).await;
    let tag_id = Uuid::now_v7();
    tag::create(
        &db.pool,
        tag_id,
        org_id,
        "Christmasy",
        Some("occasion"),
        None,
    )
    .await
    .expect("tag");
    let link = Uuid::now_v7();
    tag::attach(&db.pool, link, id, tag_id)
        .await
        .expect("attach");
    assert_eq!(
        search_titles(&app, &token, org_id, "q=christmasy").await,
        vec!["Fanfare"]
    );

    // Rename the tag: the old word must stop matching, the new one start.
    tag::update(&db.pool, tag_id, "Festive", Some("occasion"))
        .await
        .expect("rename tag");
    assert!(
        search_titles(&app, &token, org_id, "q=christmasy")
            .await
            .is_empty(),
        "the old tag word is gone from the vector"
    );
    assert_eq!(
        search_titles(&app, &token, org_id, "q=festive").await,
        vec!["Fanfare"]
    );

    // Detaching removes it again.
    tag::detach(&db.pool, id, tag_id).await.expect("detach");
    assert!(
        search_titles(&app, &token, org_id, "q=festive")
            .await
            .is_empty(),
        "a detached tag stops contributing"
    );
}

// ---------------------------------------------------------------------------
// Facets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn facets_narrow_the_result_and_compose_with_the_query() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;

    seed_searchable(
        &db.pool,
        org_id,
        "Easy Fanfare",
        None,
        None,
        None,
        Some(2),
        Some(120),
    )
    .await;
    seed_searchable(
        &db.pool,
        org_id,
        "Hard Fanfare",
        None,
        None,
        None,
        Some(7),
        Some(600),
    )
    .await;
    seed_searchable(
        &db.pool,
        org_id,
        "Hard Waltz",
        None,
        None,
        None,
        Some(7),
        Some(240),
    )
    .await;

    assert_eq!(
        search_titles(&app, &token, org_id, "filter[difficultyMin]=5").await,
        vec!["Hard Fanfare", "Hard Waltz"]
    );
    assert_eq!(
        search_titles(&app, &token, org_id, "filter[difficultyMax]=3").await,
        vec!["Easy Fanfare"]
    );
    assert_eq!(
        search_titles(&app, &token, org_id, "filter[durationMaxSeconds]=200").await,
        vec!["Easy Fanfare"]
    );
    // Facets AND with each other and with the query — the conductor's actual
    // question is all of these at once.
    assert_eq!(
        search_titles(
            &app,
            &token,
            org_id,
            "q=fanfare&filter[difficultyMin]=5&filter[durationMaxSeconds]=900"
        )
        .await,
        vec!["Hard Fanfare"]
    );
}

#[tokio::test]
async fn several_tags_must_all_be_present() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let both = seed_searchable(
        &db.pool,
        org_id,
        "Festive Brass",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    let one = seed_searchable(
        &db.pool,
        org_id,
        "Festive Strings",
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let festive = Uuid::now_v7();
    let brass = Uuid::now_v7();
    tag::create(&db.pool, festive, org_id, "Festive", Some("mood"), None)
        .await
        .unwrap();
    tag::create(&db.pool, brass, org_id, "Brass", Some("style"), None)
        .await
        .unwrap();
    tag::attach(&db.pool, Uuid::now_v7(), both, festive)
        .await
        .unwrap();
    tag::attach(&db.pool, Uuid::now_v7(), both, brass)
        .await
        .unwrap();
    tag::attach(&db.pool, Uuid::now_v7(), one, festive)
        .await
        .unwrap();

    assert_eq!(
        search_titles(&app, &token, org_id, &format!("filter[tag]={festive}")).await,
        vec!["Festive Brass", "Festive Strings"]
    );
    assert_eq!(
        search_titles(
            &app,
            &token,
            org_id,
            &format!("filter[tag]={festive},{brass}")
        )
        .await,
        vec!["Festive Brass"],
        "several tags AND together, like every other facet"
    );
}

#[tokio::test]
async fn instrumentation_filters_by_a_live_voice() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    let with_harp =
        seed_searchable(&db.pool, org_id, "Harp Piece", None, None, None, None, None).await;
    seed_searchable(&db.pool, org_id, "No Harp", None, None, None, None, None).await;
    let instrument = first_instrument_id(&db.pool).await;
    let voice_id = Uuid::now_v7();
    voice::create(
        &db.pool, voice_id, with_harp, "Harp 1", "harp-1", instrument, None,
    )
    .await
    .expect("voice");

    assert_eq!(
        search_titles(
            &app,
            &token,
            org_id,
            &format!("filter[instrumentId]={instrument}")
        )
        .await,
        vec!["Harp Piece"]
    );

    // A soft-deleted voice does not keep the arrangement in the result.
    voice::soft_delete(&db.pool, voice_id).await.unwrap();
    assert!(search_titles(
        &app,
        &token,
        org_id,
        &format!("filter[instrumentId]={instrument}")
    )
    .await
    .is_empty());
}

#[tokio::test]
async fn a_malformed_or_unknown_filter_is_a_400_not_a_silently_ignored_one() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    seed_searchable(
        &db.pool,
        org_id,
        "Anything",
        None,
        None,
        None,
        Some(3),
        None,
    )
    .await;

    // A search that quietly ignores what it did not understand and returns
    // everything is worse than one that says so.
    for query in [
        "filter[difficultyMin]=easy",
        "filter[durationMaxSeconds]=five+minutes",
        "filter[tag]=not-a-uuid",
        "filter[instrumentId]=nope",
        "filter[nonsense]=1",
        "sort=nonsense",
    ] {
        assert_eq!(
            search_status(&app, &token, org_id, query).await,
            400,
            "'{query}' must be rejected"
        );
    }

    // Percent-encoded brackets are the same filter — a browser GET form sends
    // them that way, and dropping them silently returned the unfiltered list.
    assert_eq!(
        search_status(&app, &token, org_id, "filter%5BdifficultyMin%5D=easy").await,
        400
    );
    assert_eq!(
        search_titles(&app, &token, org_id, "filter%5BdifficultyMin%5D=9")
            .await
            .len(),
        0,
        "an encoded filter actually applies"
    );
}

// ---------------------------------------------------------------------------
// Relevance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relevance_puts_the_best_match_first_and_is_ignored_without_a_query() {
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    // The word is in the title of one and only the instrumentation of the
    // other; weighting must put the title hit first.
    seed_searchable(
        &db.pool,
        org_id,
        "Zither Concerto",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    seed_searchable(
        &db.pool,
        org_id,
        "Alpine Suite",
        None,
        None,
        Some("strings and zither"),
        None,
        None,
    )
    .await;

    assert_eq!(
        search_titles(&app, &token, org_id, "q=zither&sort=relevance").await,
        vec!["Zither Concerto", "Alpine Suite"],
        "a title hit outranks an instrumentation hit"
    );

    // Bare `sort=relevance` means best-first; the generic parser would default
    // a missing direction to ascending and show the worst match first.
    let ascending_by_accident =
        search_titles(&app, &token, org_id, "q=zither&sort=relevance:asc").await;
    assert_eq!(
        ascending_by_accident,
        vec!["Alpine Suite", "Zither Concerto"]
    );

    // Without a query there is nothing to rank: fall back to the default sort
    // rather than 400 at a UI that kept the sort while clearing the box.
    assert_eq!(
        search_titles(&app, &token, org_id, "sort=relevance").await,
        vec!["Alpine Suite", "Zither Concerto"],
        "falls back to title:asc"
    );
}

#[tokio::test]
async fn fuzzy_matching_holds_at_the_default_threshold() {
    // The `%` operator reads its cutoff from `pg_trgm.similarity_threshold`
    // (default 0.3) rather than from our SQL — the operator form is the only one
    // the trigram index can serve. This pins the recall we ship with: if the
    // GUC is ever retuned, or the default changes across a Postgres version,
    // this fails rather than search quietly becoming stricter or looser.
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    seed_searchable(&db.pool, org_id, "Boléro", None, None, None, None, None).await;
    seed_searchable(
        &db.pool,
        org_id,
        "Nutcracker Suite",
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let threshold: f32 = sqlx::query_scalar("SELECT show_limit()")
        .fetch_one(&db.pool)
        .await
        .expect("read the trigram threshold");
    assert!(
        (threshold - 0.3).abs() < f32::EPSILON,
        "these expectations assume the stock 0.3 threshold, found {threshold}"
    );

    // Close enough to find.
    for typo in ["bolerro", "boler", "nutcraker"] {
        assert!(
            !search_titles(&app, &token, org_id, &format!("q={typo}"))
                .await
                .is_empty(),
            "'{typo}' should still find its piece"
        );
    }
    // Far enough away not to.
    for miss in ["trumpet", "xylophone"] {
        assert!(
            search_titles(&app, &token, org_id, &format!("q={miss}"))
                .await
                .is_empty(),
            "'{miss}' must not fuzzily match an unrelated title"
        );
    }
}

#[tokio::test]
async fn a_bad_sort_direction_is_refused_even_when_relevance_falls_back() {
    // `relevance` without a query falls back to the default sort rather than
    // erroring — but the fallback must not swallow a malformed spec on the way,
    // which it did by returning before the allowlist ran.
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;
    seed_searchable(&db.pool, org_id, "Anything", None, None, None, None, None).await;

    assert_eq!(
        search_status(&app, &token, org_id, "sort=relevance:sideways").await,
        400,
        "an unknown direction is a 400 even without a query to rank"
    );
    assert_eq!(
        search_status(&app, &token, org_id, "q=any&sort=relevance:sideways").await,
        400
    );
    // The legitimate fallback still works.
    assert_eq!(
        search_status(&app, &token, org_id, "sort=relevance").await,
        200
    );
}

#[tokio::test]
async fn a_filter_error_names_the_field_it_is_talking_about() {
    // The instrumentId branch reported "expected a tag id", copy-pasted from the
    // line above — a message that sends the reader looking in the wrong place.
    let db = TestDb::create_and_migrate().await;
    let (app, state) = build_test_app(db.pool.clone()).await;
    let (org_id, token) = org_with_member(
        &db.pool,
        &state,
        "Search Phil",
        "arch",
        membership::Role::Archivist,
    )
    .await;

    let request = bearer_request(
        "GET",
        &format!("/v1/orgs/{org_id}/arrangements?filter[instrumentId]=nope"),
        &token,
    )
    .body(axum::body::Body::empty())
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    let rendered = body.to_string();
    assert!(
        rendered.contains("an instrument id"),
        "the message must name the instrument facet, got: {rendered}"
    );
    assert!(
        !rendered.contains("a tag id"),
        "…and must not send the caller looking at tags: {rendered}"
    );
}
