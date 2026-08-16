//! Integration tests for the console coverage screens (issue #35).
//!
//! `tests/coverage.rs` covers the report through `/v1`; this covers the screens
//! — and above all the **carve-out**, since these are the only console pages a
//! non-staff user may open. The principal assertions are the ones whose failure
//! would be invisible from the outside, so they are also the ones the review
//! pass mutation-checks.
//!
//! Harness shared in shape with `tests/admin_search.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{arrangement, membership, organization, user};
use lied::state::AppState;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "console-test-password";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Ctx {
    _pg: ContainerAsync<Postgres>,
    app: axum::Router,
    state: AppState,
}

impl Ctx {
    async fn new() -> Self {
        Self::with_page_size(200).await
    }

    async fn with_page_size(max_page_size: u32) -> Self {
        let pg = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("start postgres");
        let port = pg.get_host_port_ipv4(5432).await.expect("pg port");
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&format!(
                "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
            ))
            .await
            .expect("connect pg");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");

        let state = test_state(pool.clone(), test_config(max_page_size));
        let app = lied::routes::build_router(state.clone());
        Self {
            _pg: pg,
            app,
            state,
        }
    }
}

fn test_config(max_page_size: u32) -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from("test-signing-key-material-collections".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes: 200 * 1024 * 1024,
        max_request_bytes: 256 * 1024,
        max_files_per_voice: 50,
        max_arrangements_per_org: None,
        max_page_size,
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An org with one member per role that matters here. `conductor` is the
/// interesting one: per the permission matrix it may build collections while
/// arrangements stay read-only for it.
struct Fixture {
    org_id: Uuid,
}

async fn seed(state: &AppState) -> Fixture {
    let org_id = Uuid::now_v7();
    organization::create(
        &state.db,
        org_id,
        "Console Orchestra",
        &organization::slugify("Console Orchestra"),
        None,
    )
    .await
    .expect("org");

    for (username, role) in [
        ("arch", membership::Role::Archivist),
        ("cond", membership::Role::Conductor),
        ("mus", membership::Role::Musician),
    ] {
        let uid = seed_user(state, username).await;
        membership::create(
            &state.db,
            Uuid::now_v7(),
            uid,
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

    Fixture { org_id }
}

async fn seed_user(state: &AppState, username: &str) -> Uuid {
    let hash = auth::password::hash_password(PASSWORD).expect("hash");
    let id = Uuid::now_v7();
    user::create(
        &state.db,
        id,
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
    id
}

// ---------------------------------------------------------------------------
// HTTP helpers (same shape as tests/admin_files.rs)
// ---------------------------------------------------------------------------

fn with_peer_ip<B>(mut request: axum::http::Request<B>) -> axum::http::Request<B> {
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            9000,
        ))));
    request
}

struct Browser {
    cookie: String,
}

impl Browser {
    async fn login(app: &axum::Router, username: &str) -> Self {
        let request = with_peer_ip(
            axum::http::Request::builder()
                .method("POST")
                .uri("/admin/login")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(axum::body::Body::from(format!(
                    "username={username}&password={PASSWORD}"
                )))
                .unwrap(),
        );
        let response = app.clone().oneshot(request).await.expect("login runs");
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SEE_OTHER,
            "login must redirect on success"
        );
        let cookie = response
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .find(|c| c.starts_with(auth::session::SESSION_COOKIE_NAME))
            .expect("login must set a session cookie")
            .to_string();
        Self { cookie }
    }

    fn get(&self, uri: &str) -> axum::http::Request<axum::body::Body> {
        with_peer_ip(
            axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .header(axum::http::header::COOKIE, &self.cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
    }
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn load_page(
    app: &axum::Router,
    browser: &Browser,
    uri: &str,
) -> (axum::http::StatusCode, String) {
    let response = app
        .clone()
        .oneshot(browser.get(uri))
        .await
        .expect("page request runs");
    (response.status(), body_text(response).await)
}

// ---------------------------------------------------------------------------
// Coverage fixtures
// ---------------------------------------------------------------------------

async fn two_instruments(pool: &sqlx::PgPool) -> (Uuid, Uuid) {
    let ids: Vec<Uuid> = sqlx::query_scalar!(r#"SELECT id FROM instrument ORDER BY key LIMIT 2"#)
        .fetch_all(pool)
        .await
        .expect("instrument seed");
    (ids[0], ids[1])
}

/// A programme of one piece with a flute and a trumpet voice.
async fn seed_programme(state: &AppState, org_id: Uuid) -> (Uuid, Uuid, Uuid, Uuid) {
    seed_programme_named(state, org_id, "Spring Concert").await
}

/// As above, named — an org may hold several programmes, and their slugs are
/// unique.
async fn seed_programme_named(
    state: &AppState,
    org_id: Uuid,
    name: &str,
) -> (Uuid, Uuid, Uuid, Uuid) {
    let (flute, trumpet) = two_instruments(&state.db).await;
    let collection_id = Uuid::now_v7();
    lied::domain::collection::create(
        &state.db,
        collection_id,
        org_id,
        name,
        &lied::domain::collection::slugify(name),
        "program",
        None,
    )
    .await
    .expect("collection");

    let arr_id = Uuid::now_v7();
    let arrangement_title = format!("{name} — Bolero");
    arrangement::create(
        &state.db,
        arr_id,
        org_id,
        &arrangement::slugify(&arrangement_title),
        arrangement::ArrangementFields {
            title: &arrangement_title,
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

    let flute_voice = Uuid::now_v7();
    let trumpet_voice = Uuid::now_v7();
    for (id, name, instrument) in [
        (flute_voice, "Flute 1", flute),
        (trumpet_voice, "Trumpet 1", trumpet),
    ] {
        lied::domain::voice::create(
            &state.db,
            id,
            arr_id,
            name,
            &lied::domain::voice::slugify(name),
            instrument,
            None,
        )
        .await
        .expect("voice");
    }

    let item_id = Uuid::now_v7();
    lied::domain::collection_item::create(&state.db, item_id, collection_id, arr_id, 1, None)
        .await
        .expect("item");

    (collection_id, item_id, flute_voice, flute)
}

/// Add a principal musician for `instruments` and log them in.
async fn login_principal(ctx: &Ctx, org_id: Uuid, username: &str, instruments: &[Uuid]) -> Browser {
    let hash = auth::password::hash_password(PASSWORD).expect("hash");
    let id = Uuid::now_v7();
    user::create(
        &ctx.state.db,
        id,
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
        &ctx.state.db,
        Uuid::now_v7(),
        id,
        org_id,
        membership::MembershipFields {
            role: membership::Role::Musician,
            instrument_ids: instruments,
            is_principal: true,
            principal_instrument_ids: instruments,
        },
        None,
    )
    .await
    .expect("membership");
    Browser::login(&ctx.app, username).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_dashboard_summarises_every_collection_and_links_through() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (collection_id, item_id, flute_voice, _flute) = seed_programme(&ctx.state, fx.org_id).await;
    let url = format!("/admin/orgs/{}/coverage", fx.org_id);

    // Nothing assigned yet: two gaps, called out as such.
    let (status, html) = load_page(&ctx.app, &browser, &url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("Spring Concert"));
    assert!(html.contains("0/2 assigned"), "got: {html}");
    assert!(html.contains("2 gap(s)"));
    assert!(
        html.contains(&format!("/coverage/{collection_id}")),
        "links through"
    );

    // Assign one and acknowledge it: the tallies move.
    let player = user::create(
        &ctx.state.db,
        Uuid::now_v7(),
        "player",
        "player",
        None,
        None,
        "player",
        false,
        None,
    )
    .await
    .expect("user");
    let (assignment, _) = lied::domain::part_assignment::assign(
        &ctx.state.db,
        Uuid::now_v7(),
        item_id,
        flute_voice,
        player.id,
        None,
    )
    .await
    .expect("assign");
    lied::domain::part_assignment::update_state(
        &ctx.state.db,
        assignment.id,
        None,
        Some(chrono::Utc::now()),
    )
    .await
    .expect("acknowledge");

    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    assert!(
        html.contains("1/2 assigned") && html.contains("1 rehearsed"),
        "got: {html}"
    );
}

#[tokio::test]
async fn the_detail_screen_names_each_voices_state() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (collection_id, item_id, flute_voice, _flute) = seed_programme(&ctx.state, fx.org_id).await;
    let url = format!("/admin/orgs/{}/coverage/{collection_id}", fx.org_id);

    let (status, html) = load_page(&ctx.app, &browser, &url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("Flute 1") && html.contains("Trumpet 1"));
    assert_eq!(html.matches("unassigned").count(), 2);
    // Staff can act on a gap, so they get the way to do it.
    assert!(
        html.contains(&format!("/items/{item_id}/assignments")),
        "staff get a link to fix the gap"
    );

    // Assigned but not acknowledged, and not yet notified: the two mid-states
    // are distinguishable, because they need different chasing.
    let player = user::create(
        &ctx.state.db,
        Uuid::now_v7(),
        "player",
        "player",
        None,
        None,
        "player",
        false,
        None,
    )
    .await
    .expect("user");
    let (assignment, _) = lied::domain::part_assignment::assign(
        &ctx.state.db,
        Uuid::now_v7(),
        item_id,
        flute_voice,
        player.id,
        None,
    )
    .await
    .expect("assign");
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    assert!(html.contains("assigned, not yet notified"), "got: {html}");

    lied::domain::part_assignment::update_state(
        &ctx.state.db,
        assignment.id,
        Some(chrono::Utc::now()),
        None,
    )
    .await
    .expect("notify");
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    assert!(html.contains("notified, not acknowledged"), "got: {html}");
}

#[tokio::test]
async fn a_principal_sees_only_their_section_and_no_way_to_assign() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let (collection_id, item_id, _flute_voice, flute) = seed_programme(&ctx.state, fx.org_id).await;
    let principal = login_principal(&ctx, fx.org_id, "flute-lead", &[flute]).await;

    // The carve-out: a non-staff user reaching a console screen at all.
    let (status, html) = load_page(
        &ctx.app,
        &principal,
        &format!("/admin/orgs/{}/coverage", fx.org_id),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("your own section"), "the scope is stated");
    assert!(
        html.contains("0/1 assigned"),
        "one flute voice required, not the trumpet: {html}"
    );

    let (status, html) = load_page(
        &ctx.app,
        &principal,
        &format!("/admin/orgs/{}/coverage/{collection_id}", fx.org_id),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("Flute 1"), "their own voice is shown");
    assert!(
        !html.contains("Trumpet 1"),
        "another section's voice must not appear: {html}"
    );
    // They cannot assign, so they are not offered a link that would 403.
    assert!(
        !html.contains(&format!("/items/{item_id}/assignments")),
        "a principal gets no assignment link"
    );
}

#[tokio::test]
async fn a_plain_musician_cannot_reach_coverage_at_all() {
    // NOTE on what this does and does not pin. A plain musician is refused by
    // the *console entry* gate (#30: only staff and principals may enter at
    // all), so this test would still pass if coverage's own scope rule were
    // broken — verified by mutation. The coverage-specific denial is pinned by
    // `a_plain_musician_is_refused_...` in tests/coverage.rs, which goes through
    // `/v1` where console entry is not in the way. Kept because defence in depth
    // is worth asserting; do not read it as covering the scope rule.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let (collection_id, _item, _voice, _flute) = seed_programme(&ctx.state, fx.org_id).await;
    // `seed` already creates a plain (non-principal) musician.
    let musician = Browser::login(&ctx.app, "mus").await;

    for url in [
        format!("/admin/orgs/{}/coverage", fx.org_id),
        format!("/admin/orgs/{}/coverage/{collection_id}", fx.org_id),
    ] {
        let (status, _) = load_page(&ctx.app, &musician, &url).await;
        assert_eq!(
            status,
            axum::http::StatusCode::FORBIDDEN,
            "{url} must be closed to a musician who leads nothing"
        );
    }
}

#[tokio::test]
async fn a_collection_from_another_org_is_not_found() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;

    let other_org = Uuid::now_v7();
    organization::create(
        &ctx.state.db,
        other_org,
        "Rival Orchestra",
        &organization::slugify("Rival Orchestra"),
        None,
    )
    .await
    .expect("other org");
    let (theirs, _item, _voice, _flute) = seed_programme(&ctx.state, other_org).await;

    let (status, _) = load_page(
        &ctx.app,
        &browser,
        &format!("/admin/orgs/{}/coverage/{theirs}", fx.org_id),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "a coverage report names both programme and people — it must not cross orgs"
    );
}

#[tokio::test]
async fn the_dashboard_puts_the_worst_covered_programme_first() {
    // The page exists to be scanned: the programme that needs work must not be
    // below the fold, and an empty collection must not read as fully covered.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (_covered, item_id, flute_voice, _flute) = seed_programme(&ctx.state, fx.org_id).await;

    // A second programme with nothing assigned, and a third with no pieces.
    let _gap = seed_programme_named(&ctx.state, fx.org_id, "Zzz Gap Concert").await;
    let empty = Uuid::now_v7();
    lied::domain::collection::create(
        &ctx.state.db,
        empty,
        fx.org_id,
        "Aaa Empty Book",
        "aaa-empty-book",
        "standing",
        None,
    )
    .await
    .expect("empty collection");

    // Cover the first programme completely.
    let player = user::create(
        &ctx.state.db,
        Uuid::now_v7(),
        "player",
        "player",
        None,
        None,
        "player",
        false,
        None,
    )
    .await
    .expect("user");
    for voice in [flute_voice] {
        lied::domain::part_assignment::assign(
            &ctx.state.db,
            Uuid::now_v7(),
            item_id,
            voice,
            player.id,
            None,
        )
        .await
        .expect("assign");
    }

    let (_, html) = load_page(
        &ctx.app,
        &browser,
        &format!("/admin/orgs/{}/coverage", fx.org_id),
    )
    .await;

    let position = |needle: &str| html.find(needle).unwrap_or(usize::MAX);
    assert!(
        position("Zzz Gap Concert") < position("Spring Concert"),
        "the uncovered programme outranks the half-covered one despite the name order"
    );
    assert!(
        position("Spring Concert") < position("Aaa Empty Book"),
        "a collection requiring nothing sinks below real programmes"
    );
    assert!(
        html.contains("nothing required"),
        "an empty collection says so rather than showing 0/0 as covered"
    );
}

#[tokio::test]
async fn a_conductor_gets_the_full_programme_and_the_assignment_links() {
    // Conductors are staff for coverage and may build collections, but are
    // read-only over the catalogue — a distinction that has been got wrong
    // before, so it is pinned here.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let (collection_id, item_id, _voice, _flute) = seed_programme(&ctx.state, fx.org_id).await;
    let conductor = Browser::login(&ctx.app, "cond").await;

    let (status, html) = load_page(
        &ctx.app,
        &conductor,
        &format!("/admin/orgs/{}/coverage/{collection_id}", fx.org_id),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        html.contains("Flute 1") && html.contains("Trumpet 1"),
        "a conductor sees the whole programme, not a section"
    );
    assert!(
        !html.contains("your own section"),
        "…and is not told they are seeing a section"
    );
    assert!(
        html.contains(&format!("/items/{item_id}/assignments")),
        "a conductor may assign parts, so the link is offered"
    );
}
