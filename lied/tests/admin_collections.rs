//! Integration tests for issue #33 (console collection & part-assignment
//! screens), driving the `/admin` tree over real HTTP with a session cookie and
//! the CSRF contract the pages actually render.
//!
//! The harness mirrors `tests/admin_files.rs` but needs **Postgres only** — no
//! object storage is involved in program building — so these run faster than
//! the file screens' tests.
//!
//! What is covered here is the console-specific half; `/v1` collection and
//! part-assignment behaviour is already covered by `tests/collections.rs` and
//! `tests/part_assignments.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{arrangement, collection, membership, organization, user};
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
    pool: PgPool,
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
            pool,
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

async fn seed_arrangement(state: &AppState, org_id: Uuid, title: &str) -> Uuid {
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

    fn post(&self, uri: &str, body: String) -> axum::http::Request<axum::body::Body> {
        with_peer_ip(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header(axum::http::header::COOKIE, &self.cookie)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(axum::body::Body::from(body))
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

/// The CSRF token a page embeds in its hidden field.
fn form_csrf(html: &str) -> String {
    let marker = "name=\"csrf_token\" value=\"";
    let start = html.find(marker).expect("page must render a csrf field") + marker.len();
    html[start..]
        .split('"')
        .next()
        .expect("token is quoted")
        .to_string()
}

/// The session's CSRF token from the readable cookie the middleware sets on
/// safe requests — for callers whose page renders no form.
async fn session_csrf(app: &axum::Router, browser: &Browser, uri: &str) -> String {
    let response = app
        .clone()
        .oneshot(browser.get(uri))
        .await
        .expect("page request runs");
    response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find_map(|c| c.strip_prefix("lied_csrf="))
        .expect("safe requests must hand the client a readable CSRF cookie")
        .to_string()
}

async fn post_form(
    app: &axum::Router,
    browser: &Browser,
    uri: &str,
    fields: &[(&str, &str)],
    token: &str,
) -> axum::response::Response {
    let mut body = format!("csrf_token={token}");
    for (k, v) in fields {
        body.push('&');
        body.push_str(&form_urlencoded(k, v));
    }
    app.clone()
        .oneshot(browser.post(uri, body))
        .await
        .expect("form post runs")
}

fn form_urlencoded(key: &str, value: &str) -> String {
    let encode = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".to_string(),
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };
    format!("{}={}", encode(key), encode(value))
}

async fn audit_count(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar(r#"SELECT count(*) FROM audit_log WHERE action = $1"#)
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Create a collection through the console, returning its id.
async fn create_collection(
    ctx: &Ctx,
    browser: &Browser,
    org_id: Uuid,
    name: &str,
    kind: &str,
) -> Uuid {
    let list_url = format!("/admin/orgs/{org_id}/collections");
    let (_, html) = load_page(&ctx.app, browser, &list_url).await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        browser,
        &list_url,
        &[("name", name), ("type", kind)],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SEE_OTHER,
        "creating {name} should succeed"
    );
    let (rows, _) = collection::list_for_org(
        &ctx.pool,
        org_id,
        50,
        0,
        "name",
        lied::listing::SortDirection::Asc,
        None,
    )
    .await
    .unwrap();
    rows.into_iter()
        .find(|c| c.name == name)
        .expect("created collection is listed")
        .id
}

// ---------------------------------------------------------------------------
// Tests — slice 1: collection CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_archivist_creates_edits_deletes_and_restores_a_collection() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let list_url = format!("/admin/orgs/{}/collections", fx.org_id);

    let (status, html) = load_page(&ctx.app, &browser, &list_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("No collections yet."), "empty state renders");

    let id = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    assert_eq!(audit_count(&ctx.pool, "collection.create").await, 1);

    let created = collection::find_by_id(&ctx.pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(created.slug, "spring-concert", "slug derives from the name");
    assert_eq!(created.collection_type, "program");

    // Edit: name and type are mutable, the slug is not.
    let detail_url = format!("/admin/orgs/{}/collections/{id}", fx.org_id);
    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &detail_url,
        &[
            ("name", "Spring Gala"),
            ("type", "standing"),
            (
                "expected_version",
                &created.updated_at.timestamp_millis().to_string(),
            ),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let updated = collection::find_by_id(&ctx.pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.name, "Spring Gala");
    assert_eq!(updated.collection_type, "standing");
    assert_eq!(updated.slug, "spring-concert", "the slug is immutable");
    assert_eq!(audit_count(&ctx.pool, "collection.update").await, 1);

    // Delete → gone from the live list, offered for restore.
    let (_, html) = load_page(&ctx.app, &browser, &list_url).await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/delete"),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let (_, html) = load_page(&ctx.app, &browser, &list_url).await;
    assert!(html.contains("Recently deleted"), "restore list appears");
    assert!(html.contains(&format!("{id}/undelete")));
    assert_eq!(audit_count(&ctx.pool, "collection.soft_delete").await, 1);

    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/undelete"),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(
        collection::find_by_id(&ctx.pool, id)
            .await
            .unwrap()
            .is_some(),
        "restore brings the collection back"
    );
    assert_eq!(audit_count(&ctx.pool, "collection.undelete").await, 1);
}

#[tokio::test]
async fn a_stale_edit_is_refused_and_a_duplicate_slug_conflicts() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let id = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{id}", fx.org_id);

    // Someone else's edit lands first, so the version in this form is stale.
    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    let stale_version = collection::find_by_id(&ctx.pool, id)
        .await
        .unwrap()
        .unwrap()
        .updated_at
        .timestamp_millis()
        - 1;
    let response = post_form(
        &ctx.app,
        &browser,
        &detail_url,
        &[
            ("name", "Hijacked"),
            ("type", "program"),
            ("expected_version", &stale_version.to_string()),
        ],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED,
        "a stale expected_version must not overwrite a newer edit"
    );
    let html = body_text(response).await;
    assert!(html.contains("changed by someone else"), "got: {html}");
    assert_eq!(
        collection::find_by_id(&ctx.pool, id)
            .await
            .unwrap()
            .unwrap()
            .name,
        "Spring Concert",
        "the refused edit changed nothing"
    );

    // A second collection whose name slugifies the same way collides.
    let list_url = format!("/admin/orgs/{}/collections", fx.org_id);
    let (_, html) = load_page(&ctx.app, &browser, &list_url).await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &list_url,
        &[("name", "spring concert"), ("type", "standing")],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::CONFLICT,
        "the per-org slug uniqueness must surface as a friendly conflict"
    );
}

#[tokio::test]
async fn an_invalid_type_or_empty_name_is_rejected() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let list_url = format!("/admin/orgs/{}/collections", fx.org_id);
    let (_, html) = load_page(&ctx.app, &browser, &list_url).await;
    let token = form_csrf(&html);

    for (fields, why) in [
        (
            vec![("name", "Fine"), ("type", "mixtape")],
            "a type outside the vocabulary",
        ),
        (vec![("name", "   "), ("type", "program")], "a blank name"),
        (vec![("name", "Fine"), ("type", "")], "a missing type"),
    ] {
        let response = post_form(&ctx.app, &browser, &list_url, &fields, &token).await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "{why} must be rejected"
        );
    }
    let (rows, _) = collection::list_for_org(
        &ctx.pool,
        fx.org_id,
        50,
        0,
        "name",
        lied::listing::SortDirection::Asc,
        None,
    )
    .await
    .unwrap();
    assert!(rows.is_empty(), "no collection was created by a bad form");
}

#[tokio::test]
async fn a_conductor_may_build_collections_and_a_musician_may_not() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let list_url = format!("/admin/orgs/{}/collections", fx.org_id);

    // The matrix row this issue exists to prove: conductors build programs.
    let conductor = Browser::login(&ctx.app, "cond").await;
    let (status, _) = load_page(&ctx.app, &conductor, &list_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let id = create_collection(&ctx, &conductor, fx.org_id, "Autumn Concert", "program").await;
    assert!(collection::find_by_id(&ctx.pool, id)
        .await
        .unwrap()
        .is_some());

    // …while a plain musician cannot even see the section.
    let musician = Browser::login(&ctx.app, "mus").await;
    let (status, _) = load_page(&ctx.app, &musician, &list_url).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let token = session_csrf(&ctx.app, &musician, "/admin").await;
    let response = post_form(
        &ctx.app,
        &musician,
        &list_url,
        &[("name", "Sneaky"), ("type", "program")],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "the write route refuses a musician with a valid CSRF token — \
         so this pins authorization, not the CSRF check"
    );
}

#[tokio::test]
async fn a_collection_id_from_another_org_is_not_found() {
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
    let foreign = collection::create(
        &ctx.state.db,
        Uuid::now_v7(),
        other_org,
        "Their Concert",
        "their-concert",
        "program",
        None,
    )
    .await
    .expect("foreign collection");
    // Something of ours must exist too, so the 404s can't come from an empty org.
    seed_arrangement(&ctx.state, fx.org_id, "Overture").await;

    let base = format!("/admin/orgs/{}/collections/{}", fx.org_id, foreign.id);
    let (status, _) = load_page(&ctx.app, &browser, &base).await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "another org's collection must not render through our org's URL"
    );

    let token = session_csrf(
        &ctx.app,
        &browser,
        &format!("/admin/orgs/{}/collections", fx.org_id),
    )
    .await;
    for (uri, fields) in [
        (base.clone(), vec![("name", "Stolen"), ("type", "program")]),
        (format!("{base}/delete"), vec![]),
        (format!("{base}/undelete"), vec![]),
    ] {
        let response = post_form(&ctx.app, &browser, &uri, &fields, &token).await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "{uri} must not reach another org's collection"
        );
    }
    assert_eq!(
        collection::find_by_id(&ctx.pool, foreign.id)
            .await
            .unwrap()
            .unwrap()
            .name,
        "Their Concert",
        "the foreign collection is untouched"
    );
}

// ---------------------------------------------------------------------------
// Tests — slice 2: items & ordering
// ---------------------------------------------------------------------------

/// Add an arrangement to a collection through the console.
async fn add_piece(ctx: &Ctx, browser: &Browser, org_id: Uuid, coll: Uuid, arr: Uuid) {
    let detail_url = format!("/admin/orgs/{org_id}/collections/{coll}");
    let (_, html) = load_page(&ctx.app, browser, &detail_url).await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        browser,
        &format!("{detail_url}/items"),
        &[("arrangement_id", &arr.to_string())],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SEE_OTHER,
        "adding a piece should succeed"
    );
}

/// The collection's live items in index order.
async fn items(ctx: &Ctx, coll: Uuid) -> Vec<lied::domain::collection_item::CollectionItemView> {
    lied::domain::collection_item::list_for_collection(&ctx.pool, coll)
        .await
        .unwrap()
}

#[tokio::test]
async fn pieces_are_added_numbered_reordered_and_removed() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);

    let bolero = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let egmont = seed_arrangement(&ctx.state, fx.org_id, "Egmont Overture").await;
    let fifth = seed_arrangement(&ctx.state, fx.org_id, "Symphony No. 5").await;
    for arr in [bolero, egmont, fifth] {
        add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
    }

    // Appended in order, numbered from 1 with no gaps.
    let live = items(&ctx, coll).await;
    assert_eq!(live.len(), 3);
    assert_eq!(
        live.iter().map(|i| i.item.index).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "pieces append at the next free number"
    );
    assert_eq!(live[0].arrangement_title, "Bolero");
    assert_eq!(audit_count(&ctx.pool, "collection_item.create").await, 3);

    // ▼ on the first piece swaps it with the second.
    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    let order: Vec<String> = live.iter().map(|i| i.item.id.to_string()).collect();
    let mut fields: Vec<(&str, &str)> = order.iter().map(|id| ("order", id.as_str())).collect();
    let directive = format!("down:{}", live[0].item.id);
    fields.push(("move", &directive));
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/reorder"),
        &fields,
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

    let reordered = items(&ctx, coll).await;
    assert_eq!(
        reordered
            .iter()
            .map(|i| i.arrangement_title.as_str())
            .collect::<Vec<_>>(),
        vec!["Egmont Overture", "Bolero", "Symphony No. 5"],
        "▼ moved the first piece down one slot"
    );
    assert_eq!(
        reordered.iter().map(|i| i.item.index).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "reordering renumbers from 1 rather than leaving gaps"
    );
    assert_eq!(audit_count(&ctx.pool, "collection_item.reorder").await, 1);

    // Remove the middle piece, then restore it.
    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    let victim = reordered[1].item.id;
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/{victim}/delete"),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(items(&ctx, coll).await.len(), 2);

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    assert!(html.contains("Removed pieces"), "the restore list appears");
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/{victim}/undelete"),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(items(&ctx, coll).await.len(), 3, "the piece came back");
    assert_eq!(
        audit_count(&ctx.pool, "collection_item.soft_delete").await,
        1
    );
    assert_eq!(audit_count(&ctx.pool, "collection_item.undelete").await, 1);
}

#[tokio::test]
async fn the_end_pieces_have_no_arrow_off_the_end_and_a_forged_one_is_refused() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    for title in ["Bolero", "Egmont Overture"] {
        let arr = seed_arrangement(&ctx.state, fx.org_id, title).await;
        add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
    }
    let live = items(&ctx, coll).await;

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    assert!(
        !html.contains(&format!("up:{}", live[0].item.id)),
        "the first piece is not offered a ▲"
    );
    assert!(
        !html.contains(&format!("down:{}", live[1].item.id)),
        "the last piece is not offered a ▼"
    );

    // The page hides those arrows; a hand-crafted POST must still be refused.
    let token = form_csrf(&html);
    let order: Vec<String> = live.iter().map(|i| i.item.id.to_string()).collect();
    let directive = format!("up:{}", live[0].item.id);
    let mut fields: Vec<(&str, &str)> = order.iter().map(|id| ("order", id.as_str())).collect();
    fields.push(("move", &directive));
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/reorder"),
        &fields,
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        items(&ctx, coll).await[0].item.id,
        live[0].item.id,
        "the refused move changed nothing"
    );
}

#[tokio::test]
async fn an_order_that_no_longer_matches_the_collection_is_refused() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    for title in ["Bolero", "Egmont Overture"] {
        let arr = seed_arrangement(&ctx.state, fx.org_id, title).await;
        add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
    }
    let live = items(&ctx, coll).await;

    // This page is now stale: a third piece lands before the reorder is
    // submitted, so the submitted order isn't the live set any more.
    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    let third = seed_arrangement(&ctx.state, fx.org_id, "Symphony No. 5").await;
    add_piece(&ctx, &browser, fx.org_id, coll, third).await;

    let order: Vec<String> = live.iter().map(|i| i.item.id.to_string()).collect();
    let directive = format!("down:{}", live[0].item.id);
    let mut fields: Vec<(&str, &str)> = order.iter().map(|id| ("order", id.as_str())).collect();
    fields.push(("move", &directive));
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/reorder"),
        &fields,
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::CONFLICT,
        "a partial order must not silently drop the piece it omits"
    );
    let html = body_text(response).await;
    assert!(
        html.contains("changed since the page was loaded"),
        "got: {html}"
    );
    assert_eq!(
        items(&ctx, coll).await.len(),
        3,
        "all three pieces are still in the collection"
    );
}

#[tokio::test]
async fn a_piece_whose_arrangement_was_deleted_renders_as_removed() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    add_piece(&ctx, &browser, fx.org_id, coll, arr).await;

    // Soft-deleting the arrangement must not silently drop it from the program:
    // hide-with-references means the slot stays, marked.
    arrangement::soft_delete(&ctx.pool, arr).await.unwrap();

    let (status, html) = load_page(&ctx.app, &browser, &detail_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        html.contains("[removed]") && html.contains("bolero"),
        "a deleted arrangement shows as [removed] with its slug, got: {html}"
    );
    assert!(
        !html.contains("Bolero</td>"),
        "the deleted arrangement's title is not rendered as a live entry"
    );
    assert_eq!(
        items(&ctx, coll).await.len(),
        1,
        "the item itself is untouched by the arrangement's deletion"
    );
}

#[tokio::test]
async fn an_arrangement_from_another_org_cannot_be_added() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);

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
    let foreign = seed_arrangement(&ctx.state, other_org, "Their Secret Piece").await;

    let token = session_csrf(&ctx.app, &browser, &detail_url).await;
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items"),
        &[("arrangement_id", &foreign.to_string())],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::NOT_FOUND,
        "a forged arrangement id must not pull another org's piece into the program"
    );
    assert!(
        items(&ctx, coll).await.is_empty(),
        "nothing was added to the collection"
    );
}
