//! Integration tests for the console archive-search screen (issue #34).
//!
//! `tests/search.rs` covers the search itself through `/v1`; this covers the
//! screen the conductor actually uses — that its form reaches the same search,
//! that a facet it cannot parse is reported rather than dropped, and that it
//! stays staff-only.
//!
//! Harness shared in shape with `tests/admin_collections.rs`.

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
// Console archive search (issue #34, slice 3) — the conductor's planning screen
// ---------------------------------------------------------------------------

async fn seed_searchable_arrangement(
    state: &AppState,
    org_id: Uuid,
    title: &str,
    difficulty: Option<i16>,
    duration_seconds: Option<i32>,
) -> Uuid {
    let id = Uuid::now_v7();
    arrangement::create(
        &state.db,
        id,
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
            duration_seconds,
            difficulty,
            difficulty_ratings: None,
            difficulty_notes: None,
        },
        None,
    )
    .await
    .expect("arrangement");
    id
}

#[tokio::test]
async fn the_search_screen_finds_pieces_and_narrows_by_facet() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "cond").await;
    let search_url = format!("/admin/orgs/{}/search", fx.org_id);

    seed_searchable_arrangement(&ctx.state, fx.org_id, "Boléro", Some(6), Some(900)).await;
    seed_searchable_arrangement(&ctx.state, fx.org_id, "Easy Fanfare", Some(2), Some(120)).await;

    // An untouched form browses the whole catalogue.
    let (status, html) = load_page(&ctx.app, &browser, &search_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("All arrangements"));
    assert!(html.contains("Boléro") && html.contains("Easy Fanfare"));

    // The query box reaches the same search /v1 does — accents optional.
    let (_, html) = load_page(&ctx.app, &browser, &format!("{search_url}?q=bolero")).await;
    assert!(html.contains("Boléro"), "accent-folded match, got: {html}");
    assert!(!html.contains("Easy Fanfare"));

    // Facets arrive percent-encoded from a browser GET form; they must apply.
    let (_, html) = load_page(
        &ctx.app,
        &browser,
        &format!("{search_url}?difficulty_min=5"),
    )
    .await;
    assert!(html.contains("Boléro") && !html.contains("Easy Fanfare"));

    // A misspelling still finds it — the reason trigram matching is there.
    let (_, html) = load_page(&ctx.app, &browser, &format!("{search_url}?q=bolerro")).await;
    assert!(html.contains("Boléro"));

    // Nothing matched reads as nothing matched, not as an error.
    let (_, html) = load_page(&ctx.app, &browser, &format!("{search_url}?q=tubaconcerto")).await;
    assert!(html.contains("Nothing matched"), "got: {html}");
}

#[tokio::test]
async fn a_search_facet_that_cannot_be_parsed_is_reported_not_silently_dropped() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let search_url = format!("/admin/orgs/{}/search", fx.org_id);
    seed_searchable_arrangement(&ctx.state, fx.org_id, "Boléro", Some(6), None).await;

    // The console keeps rendering (unlike /v1, which 400s) but must say which
    // box it ignored — a search that quietly drops a filter and returns
    // everything is the failure mode this screen must not have.
    let (status, html) = load_page(
        &ctx.app,
        &browser,
        &format!("{search_url}?difficulty_min=easy"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        html.contains("that filter was ignored"),
        "the page must disclose the ignored filter, got: {html}"
    );
    assert!(html.contains("Boléro"), "the rest of the search still ran");
}

#[tokio::test]
async fn the_search_screen_is_staff_only() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let search_url = format!("/admin/orgs/{}/search", fx.org_id);

    // Search is the conductor's planning surface, so a conductor gets it…
    let conductor = Browser::login(&ctx.app, "cond").await;
    let (status, _) = load_page(&ctx.app, &conductor, &search_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // …while a plain musician does not: they reach their parts over WebDAV.
    let musician = Browser::login(&ctx.app, "mus").await;
    let (status, _) = load_page(&ctx.app, &musician, &search_url).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_unparseable_id_facet_is_reported_like_the_numeric_ones() {
    // The id facets come from `<select>`s, so a bad value means a hand-edited
    // URL — which is exactly when being told beats being quietly ignored. They
    // used to be dropped with `.ok()` while the numeric facets complained.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let search_url = format!("/admin/orgs/{}/search", fx.org_id);
    seed_searchable_arrangement(&ctx.state, fx.org_id, "Boléro", Some(6), None).await;

    for query in ["tag=not-a-uuid", "instrument_id=not-a-uuid"] {
        let (status, html) = load_page(&ctx.app, &browser, &format!("{search_url}?{query}")).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(
            html.contains("is not a valid id"),
            "'{query}' must be reported, got: {html}"
        );
        assert!(
            html.contains("Boléro"),
            "the rest of the search still runs after an ignored facet"
        );
    }
}

#[tokio::test]
async fn the_tag_facet_survives_a_multi_select_submission() {
    // A `<select multiple>` submits its key once per selection. Reading the
    // form as a typed struct made that a 400 from the extractor — before any
    // handler code ran — so the tag facet was unusable from the screen it is
    // rendered on.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let search_url = format!("/admin/orgs/{}/search", fx.org_id);

    let both =
        seed_searchable_arrangement(&ctx.state, fx.org_id, "Festive Brass", None, None).await;
    let one =
        seed_searchable_arrangement(&ctx.state, fx.org_id, "Festive Strings", None, None).await;
    let festive = Uuid::now_v7();
    let brass = Uuid::now_v7();
    lied::domain::tag::create(
        &ctx.state.db,
        festive,
        fx.org_id,
        "Festive",
        Some("mood"),
        None,
    )
    .await
    .unwrap();
    lied::domain::tag::create(
        &ctx.state.db,
        brass,
        fx.org_id,
        "Brass",
        Some("style"),
        None,
    )
    .await
    .unwrap();
    for (arr, tag) in [(both, festive), (both, brass), (one, festive)] {
        lied::domain::tag::attach(&ctx.state.db, Uuid::now_v7(), arr, tag)
            .await
            .unwrap();
    }

    let (status, html) =
        load_page(&ctx.app, &browser, &format!("{search_url}?tag={festive}")).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("Festive Brass") && html.contains("Festive Strings"));

    // Two selections: the repeated key must survive, and AND.
    let (status, html) = load_page(
        &ctx.app,
        &browser,
        &format!("{search_url}?tag={festive}&tag={brass}"),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "a repeated key must not fail the extractor"
    );
    assert!(html.contains("Festive Brass"));
    assert!(
        !html.contains("Festive Strings"),
        "several tags AND together, as they do on /v1"
    );
}
