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

// ---------------------------------------------------------------------------
// Tests — slice 3: part assignments
// ---------------------------------------------------------------------------

async fn seed_voice(state: &AppState, arr_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    let instrument_id = sqlx::query_scalar!(r#"SELECT id FROM instrument ORDER BY key LIMIT 1"#)
        .fetch_one(&state.db)
        .await
        .expect("instrument");
    lied::domain::voice::create(
        &state.db,
        id,
        arr_id,
        name,
        &lied::domain::voice::slugify(name),
        instrument_id,
        None,
    )
    .await
    .expect("voice");
    id
}

/// A collection with one piece that has two voices — the shape the assignment
/// matrix is built for. Returns (collection, item, voice ids).
async fn seed_program_with_voices(
    ctx: &Ctx,
    browser: &Browser,
    org_id: Uuid,
) -> (Uuid, Uuid, Vec<Uuid>) {
    let coll = create_collection(ctx, browser, org_id, "Spring Concert", "program").await;
    let arr = seed_arrangement(&ctx.state, org_id, "Bolero").await;
    let flute = seed_voice(&ctx.state, arr, "Flute 1").await;
    let trumpet = seed_voice(&ctx.state, arr, "Trumpet 1").await;
    add_piece(ctx, browser, org_id, coll, arr).await;
    let item = items(ctx, coll).await[0].item.id;
    (coll, item, vec![flute, trumpet])
}

async fn assignment_for(
    ctx: &Ctx,
    item: Uuid,
    voice: Uuid,
) -> Option<lied::domain::part_assignment::PartAssignment> {
    lied::domain::part_assignment::find_by_item_voice(&ctx.pool, item, voice)
        .await
        .unwrap()
}

#[tokio::test]
async fn every_voice_is_listed_and_a_guest_can_hold_a_part() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item, voices) = seed_program_with_voices(&ctx, &browser, fx.org_id).await;
    let url = format!(
        "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
        fx.org_id
    );

    // The matrix is voice-first: an unassigned voice is exactly what the
    // archivist is looking for, so it must be visible, not absent.
    let (status, html) = load_page(&ctx.app, &browser, &url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("Flute 1") && html.contains("Trumpet 1"));
    assert_eq!(html.matches("unassigned").count(), 2);

    // A substitute with NO membership in this org can take a part — that is how
    // guests get access at all (the assignment itself grants the read).
    seed_user(&ctx.state, "guest-sub").await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &url,
        &[
            ("voice_id", &voices[0].to_string()),
            ("username", "guest-sub"),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

    let assigned = assignment_for(&ctx, item, voices[0])
        .await
        .expect("assigned");
    assert!(assigned.notified_at.is_none());
    assert_eq!(audit_count(&ctx.pool, "part_assignment.create").await, 1);
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    assert!(html.contains("guest-sub"), "the assignee is shown");
    assert_eq!(
        html.matches("unassigned").count(),
        1,
        "the other voice is still open"
    );

    // Unassigning frees the voice again.
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{url}/{}/unassign", assigned.id),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(assignment_for(&ctx, item, voices[0]).await.is_none());
    assert_eq!(audit_count(&ctx.pool, "part_assignment.delete").await, 1);
}

#[tokio::test]
async fn a_reassignment_from_a_stale_form_is_refused() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item, voices) = seed_program_with_voices(&ctx, &browser, fx.org_id).await;
    let url = format!(
        "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
        fx.org_id
    );
    seed_user(&ctx.state, "first-player").await;
    seed_user(&ctx.state, "second-player").await;

    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    let token = form_csrf(&html);
    post_form(
        &ctx.app,
        &browser,
        &url,
        &[
            ("voice_id", &voices[0].to_string()),
            ("username", "first-player"),
        ],
        &token,
    )
    .await;
    let current = assignment_for(&ctx, item, voices[0]).await.unwrap();

    // A reassignment carrying a stale version must not silently overwrite an
    // assignment that changed since the page rendered.
    let stale = (current.updated_at.timestamp_millis() - 1).to_string();
    let response = post_form(
        &ctx.app,
        &browser,
        &url,
        &[
            ("voice_id", &voices[0].to_string()),
            ("username", "second-player"),
            ("expected_version", &stale),
        ],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED,
        "a stale reassignment must be refused"
    );
    assert_eq!(
        assignment_for(&ctx, item, voices[0]).await.unwrap().user_id,
        current.user_id,
        "the refused reassignment changed nothing"
    );

    // With the current version it goes through, and replaces rather than
    // duplicating (one assignee per voice per piece).
    let fresh = current.updated_at.timestamp_millis().to_string();
    let response = post_form(
        &ctx.app,
        &browser,
        &url,
        &[
            ("voice_id", &voices[0].to_string()),
            ("username", "second-player"),
            ("expected_version", &fresh),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let replaced = assignment_for(&ctx, item, voices[0]).await.unwrap();
    assert_ne!(replaced.user_id, current.user_id, "the assignee changed");
    assert_eq!(audit_count(&ctx.pool, "part_assignment.reassign").await, 1);
}

#[tokio::test]
async fn distribution_state_is_stamped_from_the_console() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item, voices) = seed_program_with_voices(&ctx, &browser, fx.org_id).await;
    let url = format!(
        "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
        fx.org_id
    );
    seed_user(&ctx.state, "player").await;
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    let token = form_csrf(&html);
    post_form(
        &ctx.app,
        &browser,
        &url,
        &[("voice_id", &voices[0].to_string()), ("username", "player")],
        &token,
    )
    .await;
    let assignment = assignment_for(&ctx, item, voices[0]).await.unwrap();

    for (action, check) in [("notified", "notified"), ("acknowledged", "acknowledged")] {
        let (_, html) = load_page(&ctx.app, &browser, &url).await;
        let token = form_csrf(&html);
        let response = post_form(
            &ctx.app,
            &browser,
            &format!("{url}/{}/{action}", assignment.id),
            &[],
            &token,
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SEE_OTHER,
            "marking {check} should succeed"
        );
    }

    let stamped = assignment_for(&ctx, item, voices[0]).await.unwrap();
    assert!(stamped.notified_at.is_some(), "notifiedAt was stamped");
    assert!(
        stamped.acknowledged_at.is_some(),
        "acknowledgedAt was stamped"
    );
    assert_eq!(
        audit_count(&ctx.pool, "part_assignment.update_state").await,
        2
    );

    // Once set, the timestamp is rendered rather than offered as a button —
    // the domain has no way to clear it back to NULL.
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    assert!(
        !html.contains("Mark notified"),
        "a stamped assignment stops offering the button"
    );
}

#[tokio::test]
async fn an_unknown_username_or_a_foreign_voice_is_refused() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item, voices) = seed_program_with_voices(&ctx, &browser, fx.org_id).await;
    let url = format!(
        "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
        fx.org_id
    );
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    let token = form_csrf(&html);

    let response = post_form(
        &ctx.app,
        &browser,
        &url,
        &[
            ("voice_id", &voices[0].to_string()),
            ("username", "nobody-by-that-name"),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let html_body = body_text(response).await;
    assert!(
        html_body.contains("No user with that username"),
        "got: {html_body}"
    );

    // A voice from a different arrangement must not be assignable on this piece
    // — the domain enforces it inside the INSERT; the console must surface it.
    let other_arr = seed_arrangement(&ctx.state, fx.org_id, "Egmont Overture").await;
    let foreign_voice = seed_voice(&ctx.state, other_arr, "Horn 1").await;
    seed_user(&ctx.state, "player").await;
    let response = post_form(
        &ctx.app,
        &browser,
        &url,
        &[
            ("voice_id", &foreign_voice.to_string()),
            ("username", "player"),
        ],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::NOT_FOUND,
        "a voice outside this piece's arrangement is not assignable"
    );
    assert!(assignment_for(&ctx, item, foreign_voice).await.is_none());
}

#[tokio::test]
async fn a_conductor_may_assign_parts_and_a_musician_may_not() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let archivist = Browser::login(&ctx.app, "arch").await;
    let (coll, item, voices) = seed_program_with_voices(&ctx, &archivist, fx.org_id).await;
    let url = format!(
        "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
        fx.org_id
    );
    seed_user(&ctx.state, "player").await;

    // Part assignments are a conductor capability per the matrix.
    let conductor = Browser::login(&ctx.app, "cond").await;
    let (status, html) = load_page(&ctx.app, &conductor, &url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &conductor,
        &url,
        &[("voice_id", &voices[0].to_string()), ("username", "player")],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(assignment_for(&ctx, item, voices[0]).await.is_some());

    // A plain musician reaches neither the screen nor the write.
    let musician = Browser::login(&ctx.app, "mus").await;
    let (status, _) = load_page(&ctx.app, &musician, &url).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let token = session_csrf(&ctx.app, &musician, "/admin").await;
    let response = post_form(
        &ctx.app,
        &musician,
        &url,
        &[("voice_id", &voices[1].to_string()), ("username", "player")],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "refused with the musician's own valid CSRF token, so this pins authorization"
    );
    assert!(assignment_for(&ctx, item, voices[1]).await.is_none());
}

#[tokio::test]
async fn an_item_or_assignment_from_another_collection_is_not_found() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item, voices) = seed_program_with_voices(&ctx, &browser, fx.org_id).await;

    // A second collection, with its own piece and assignment.
    let other = create_collection(&ctx, &browser, fx.org_id, "Autumn Concert", "program").await;
    let other_arr = seed_arrangement(&ctx.state, fx.org_id, "Egmont Overture").await;
    seed_voice(&ctx.state, other_arr, "Horn 1").await;
    add_piece(&ctx, &browser, fx.org_id, other, other_arr).await;
    let other_item = items(&ctx, other).await[0].item.id;

    // The other collection's item must not resolve under this collection's URL.
    let crossed = format!(
        "/admin/orgs/{}/collections/{coll}/items/{other_item}/assignments",
        fx.org_id
    );
    let (status, _) = load_page(&ctx.app, &browser, &crossed).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // …nor an assignment belonging to a different item.
    let url = format!(
        "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
        fx.org_id
    );
    seed_user(&ctx.state, "player").await;
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    let token = form_csrf(&html);
    post_form(
        &ctx.app,
        &browser,
        &url,
        &[("voice_id", &voices[0].to_string()), ("username", "player")],
        &token,
    )
    .await;
    let mine = assignment_for(&ctx, item, voices[0]).await.unwrap();

    let other_url = format!(
        "/admin/orgs/{}/collections/{other}/items/{other_item}/assignments",
        fx.org_id
    );
    for action in ["unassign", "notified", "acknowledged"] {
        let response = post_form(
            &ctx.app,
            &browser,
            &format!("{other_url}/{}/{action}", mine.id),
            &[],
            &token,
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "{action} must not reach an assignment on another piece"
        );
    }
    assert!(
        assignment_for(&ctx, item, voices[0]).await.is_some(),
        "the assignment is untouched"
    );
}

// ---------------------------------------------------------------------------
// Tests — review findings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reordering_a_standing_collection_renumbers_it_so_the_numbers_follow_the_order() {
    // A march book is drawn from by piece number, so the numbers have to agree
    // with the running order: reorder the book and the numbering must reflect
    // it, always increasing down the list. This holds for a standing collection
    // exactly as it does for a program — there is one ordering story.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "March Book", "standing").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    for title in ["Bolero", "Egmont Overture", "Radetzky March"] {
        let arr = seed_arrangement(&ctx.state, fx.org_id, title).await;
        add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
    }
    let live = items(&ctx, coll).await;

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    assert!(
        html.contains("value=\"down:"),
        "a standing collection is reordered with the same arrows a program uses"
    );

    // Move the last piece to the front.
    let token = form_csrf(&html);
    let order: Vec<String> = live.iter().map(|i| i.item.id.to_string()).collect();
    let directive = format!("up:{}", live[2].item.id);
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
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

    let after = items(&ctx, coll).await;
    assert_eq!(
        after
            .iter()
            .map(|i| i.arrangement_title.as_str())
            .collect::<Vec<_>>(),
        vec!["Bolero", "Radetzky March", "Egmont Overture"],
        "the third piece moved up one"
    );
    // The invariant: numbering reflects the new order, and increases.
    let numbers: Vec<i32> = after.iter().map(|i| i.item.index).collect();
    assert_eq!(numbers, vec![1, 2, 3], "the book is renumbered to match");
    assert!(
        numbers.windows(2).all(|w| w[0] < w[1]),
        "piece numbers increase down the book: {numbers:?}"
    );
}

#[tokio::test]
async fn an_empty_submitted_order_is_a_bad_request_not_a_silent_no_op() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    add_piece(&ctx, &browser, fx.org_id, coll, arr).await;

    let token = session_csrf(&ctx.app, &browser, &detail_url).await;
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/reorder"),
        &[],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::BAD_REQUEST,
        "a reorder that names no rows is malformed, not a no-op"
    );
    assert_eq!(items(&ctx, coll).await.len(), 1);
}

#[tokio::test]
async fn a_piece_whose_arrangement_was_removed_takes_no_new_assignees() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item, voices) = seed_program_with_voices(&ctx, &browser, fx.org_id).await;
    let url = format!(
        "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
        fx.org_id
    );
    seed_user(&ctx.state, "player").await;

    // Assign one part while the arrangement is live…
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    let token = form_csrf(&html);
    post_form(
        &ctx.app,
        &browser,
        &url,
        &[("voice_id", &voices[0].to_string()), ("username", "player")],
        &token,
    )
    .await;
    let existing = assignment_for(&ctx, item, voices[0]).await.unwrap();

    // …then remove the arrangement from the catalog. Its files are hidden
    // through it, so a new assignment would grant access to nothing.
    let arr_id = lied::domain::collection_item::find_by_id(&ctx.pool, item)
        .await
        .unwrap()
        .unwrap()
        .arrangement_id;
    arrangement::soft_delete(&ctx.pool, arr_id).await.unwrap();

    let (status, html) = load_page(&ctx.app, &browser, &url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        html.contains("was removed from the catalog"),
        "the screen says why it is read-only, got: {html}"
    );
    assert!(
        !html.contains("name=\"username\""),
        "no assign control is offered for a removed arrangement"
    );

    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &url,
        &[("voice_id", &voices[1].to_string()), ("username", "player")],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::CONFLICT,
        "the route holds the line even though the control is hidden"
    );
    assert!(assignment_for(&ctx, item, voices[1]).await.is_none());

    // Existing parts can still be cleaned up.
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{url}/{}/unassign", existing.id),
        &[],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SEE_OTHER,
        "unassigning stays possible so the archivist can tidy up"
    );
}

#[tokio::test]
async fn the_assignee_datalist_lists_org_members_in_one_query() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item, _) = seed_program_with_voices(&ctx, &browser, fx.org_id).await;

    // A user with no membership must not be offered — but must still be
    // assignable by typing the name (the guest path, covered elsewhere).
    seed_user(&ctx.state, "outsider").await;

    let (_, html) = load_page(
        &ctx.app,
        &browser,
        &format!(
            "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
            fx.org_id
        ),
    )
    .await;
    for member in ["arch", "cond", "mus"] {
        assert!(
            html.contains(&format!("<option value=\"{member}\">")),
            "{member} should be offered in the datalist"
        );
    }
    assert!(
        !html.contains("<option value=\"outsider\">"),
        "a non-member is not offered, though they remain assignable by name"
    );

    // The list comes from one JOIN, not a query per member.
    let usernames = lied::domain::membership::member_usernames(&ctx.pool, fx.org_id, 50)
        .await
        .unwrap();
    assert_eq!(usernames, vec!["arch", "cond", "mus"]);
}

#[tokio::test]
async fn the_reorder_audit_records_the_resulting_order() {
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
    let token = form_csrf(&html);
    let order: Vec<String> = live.iter().map(|i| i.item.id.to_string()).collect();
    let directive = format!("down:{}", live[0].item.id);
    let mut fields: Vec<(&str, &str)> = order.iter().map(|id| ("order", id.as_str())).collect();
    fields.push(("move", &directive));
    post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/reorder"),
        &fields,
        &token,
    )
    .await;

    // The audit payload must carry the order that was applied — "someone
    // reordered this" is not enough to reconstruct a concert running order.
    let payload: serde_json::Value = sqlx::query_scalar(
        r#"SELECT payload FROM audit_log WHERE action = 'collection_item.reorder'
           ORDER BY at DESC LIMIT 1"#,
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    let recorded: Vec<String> = payload["order"]
        .as_array()
        .expect("the payload records an order")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        recorded,
        vec![live[1].item.id.to_string(), live[0].item.id.to_string()],
        "the audit row records the swapped order, not the submitted one"
    );
}

#[tokio::test]
async fn an_out_of_range_piece_number_is_refused_before_it_can_break_reordering() {
    // Found by hand-testing #33: the console accepted any positive i32 while
    // `/v1` capped it at MAX_INDEX. An index near i32::MAX was therefore
    // storable from the form, and the next reorder — which parks indices at
    // `index + 1_000_000` to clear the unique space — overflowed Postgres
    // `integer` and 500'd. Both surfaces now share `is_valid_index`.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    for bad in ["2147483000", "1000000", "0", "-3"] {
        let response = post_form(
            &ctx.app,
            &browser,
            &format!("{detail_url}/items"),
            &[("arrangement_id", &arr.to_string()), ("index", bad)],
            &token,
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "piece number {bad} must be refused"
        );
    }
    assert!(
        items(&ctx, coll).await.is_empty(),
        "no piece was stored at an unusable number"
    );

    // The boundary is usable, and reordering across it still works — i.e. the
    // cap is what keeps the park-and-renumber arithmetic in range.
    let second = seed_arrangement(&ctx.state, fx.org_id, "Egmont Overture").await;
    for (arrangement, index) in [(arr, "999999"), (second, "1")] {
        let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
        let token = form_csrf(&html);
        let response = post_form(
            &ctx.app,
            &browser,
            &format!("{detail_url}/items"),
            &[
                ("arrangement_id", &arrangement.to_string()),
                ("index", index),
            ],
            &token,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    }

    let live = items(&ctx, coll).await;
    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
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
        axum::http::StatusCode::SEE_OTHER,
        "reordering a collection holding the maximum index must not overflow"
    );
    assert_eq!(
        items(&ctx, coll)
            .await
            .iter()
            .map(|i| i.item.index)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "and it renumbers as usual"
    );
}
