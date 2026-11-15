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
        s3_version_retention_days: 30,
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
        3,
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

/// The collection's live items in position order.
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
        live.iter().map(|i| i.item.position).collect::<Vec<_>>(),
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
        reordered
            .iter()
            .map(|i| i.item.position)
            .collect::<Vec<_>>(),
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
    arrangement::soft_delete(&ctx.pool, arr, None)
        .await
        .unwrap();

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
        &[instrument_id],
        lied::domain::voice::NewPartNumber::Set(None),
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

/// Everyone playing a voice on a piece — a part may have a whole section on it.
async fn players_for(
    ctx: &Ctx,
    item: Uuid,
    voice: Uuid,
) -> Vec<lied::domain::part_assignment::PartAssignment> {
    lied::domain::part_assignment::list_for_item_voice(&ctx.pool, item, voice)
        .await
        .unwrap()
}

async fn assignment_for(
    ctx: &Ctx,
    item: Uuid,
    voice: Uuid,
) -> Option<lied::domain::part_assignment::PartAssignment> {
    players_for(ctx, item, voice).await.into_iter().next()
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
    assert_eq!(html.matches("nobody yet").count(), 2);

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
        html.matches("nobody yet").count(),
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
async fn a_part_takes_several_players_and_refuses_the_same_one_twice() {
    // A second violin part is played by every desk of the section. Adding the
    // second player used to remove the first (the assignment was an upsert);
    // now it adds. The same person twice is still a mistake.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item, voices) = seed_program_with_voices(&ctx, &browser, fx.org_id).await;
    let url = format!(
        "/admin/orgs/{}/collections/{coll}/items/{item}/assignments",
        fx.org_id
    );
    for username in ["first-desk", "second-desk"] {
        seed_user(&ctx.state, username).await;
    }

    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    let token = form_csrf(&html);
    for username in ["first-desk", "second-desk"] {
        let response = post_form(
            &ctx.app,
            &browser,
            &url,
            &[("voice_id", &voices[0].to_string()), ("username", username)],
            &token,
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SEE_OTHER,
            "adding {username} should succeed"
        );
    }

    let players = players_for(&ctx, item, voices[0]).await;
    assert_eq!(players.len(), 2, "both desks hold the part");
    assert_eq!(audit_count(&ctx.pool, "part_assignment.create").await, 2);

    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    assert!(
        html.contains("first-desk") && html.contains("second-desk"),
        "both players are listed: {html}"
    );
    assert!(html.contains("2 players"), "the count is shown: {html}");

    // The same person again is a conflict, not a silent no-op or a duplicate.
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &url,
        &[
            ("voice_id", &voices[0].to_string()),
            ("username", "first-desk"),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        players_for(&ctx, item, voices[0]).await.len(),
        2,
        "the refused add changed nothing"
    );

    // Removing one player leaves the other on the part.
    let (_, html) = load_page(&ctx.app, &browser, &url).await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{url}/{}/unassign", players[0].id),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let remaining = players_for(&ctx, item, voices[0]).await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, players[1].id);
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
async fn a_standing_collection_keeps_its_numbers_and_has_no_reorder_arrows() {
    // A Marschbuch's numbers are printed in every player's folder (#102):
    // removing a piece leaves its number free, and the order follows the
    // numbers, so there is nothing to drag around.
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
    let labels: Vec<&str> = live.iter().map(|i| i.item.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["1", "2", "3"],
        "each new piece takes the next number"
    );

    remove_piece(&ctx, &browser, fx.org_id, coll, live[1].item.id).await;
    let after = items(&ctx, coll).await;
    assert_eq!(
        after
            .iter()
            .map(|i| (i.item.label.as_str(), i.item.position))
            .collect::<Vec<_>>(),
        vec![("1", 1), ("3", 2)],
        "the gap stays; positions stay dense"
    );

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    assert!(
        !html.contains("value=\"down:") && !html.contains("value=\"up:"),
        "a standing collection offers no reorder arrows"
    );
    // The next piece is offered the next number after the highest
    // (preselected), and the gap left by the removal as a choice.
    assert!(html.contains("<option value=\"4\" selected>"), "{html}");
    assert!(html.contains("<option value=\"2\">2 (free)"), "{html}");

    // A forged reorder is refused and changes nothing.
    let token = form_csrf(&html);
    let order: Vec<String> = after.iter().rev().map(|i| i.item.id.to_string()).collect();
    let fields: Vec<(&str, &str)> = order.iter().map(|id| ("order", id.as_str())).collect();
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/reorder"),
        &fields,
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        items(&ctx, coll)
            .await
            .iter()
            .map(|i| i.item.label.clone())
            .collect::<Vec<_>>(),
        vec!["1", "3"]
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
    arrangement::soft_delete(&ctx.pool, arr_id, None)
        .await
        .unwrap();

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
async fn an_unusable_position_is_refused_and_inserting_shifts_the_rest() {
    // Found by hand-testing #33: the console accepted any positive i32. Since
    // #102 a program's pieces take a position 1..n+1 (default the end), and
    // inserting moves the pieces after it down one.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    for bad in ["2147483000", "1000000", "0", "-3", "2", "x"] {
        let response = post_form(
            &ctx.app,
            &browser,
            &format!("{detail_url}/items"),
            &[("arrangement_id", &arr.to_string()), ("position", bad)],
            &token,
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "position {bad} must be refused"
        );
    }
    assert!(
        items(&ctx, coll).await.is_empty(),
        "no piece was stored at an unusable position"
    );

    let second = seed_arrangement(&ctx.state, fx.org_id, "Egmont Overture").await;
    for arrangement in [arr, second] {
        let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
        let token = form_csrf(&html);
        let response = post_form(
            &ctx.app,
            &browser,
            &format!("{detail_url}/items"),
            &[
                ("arrangement_id", &arrangement.to_string()),
                ("position", "1"),
            ],
            &token,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    }
    assert_eq!(
        items(&ctx, coll)
            .await
            .iter()
            .map(|i| (i.item.label.clone(), i.arrangement_title.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("1".to_string(), "Egmont Overture".to_string()),
            ("2".to_string(), "Bolero".to_string()),
        ],
        "the second piece went in at the front"
    );
}

// ---------------------------------------------------------------------------
// Tests — removal and restore semantics
// ---------------------------------------------------------------------------

/// Remove a piece through the console.
async fn remove_piece(ctx: &Ctx, browser: &Browser, org_id: Uuid, coll: Uuid, item: Uuid) {
    let detail_url = format!("/admin/orgs/{org_id}/collections/{coll}");
    let (_, html) = load_page(&ctx.app, browser, &detail_url).await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        browser,
        &format!("{detail_url}/items/{item}/delete"),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn removing_a_piece_closes_the_gap_immediately() {
    // Leaving 1, 2, 4, 5 until the next arrow press means the numbers on screen
    // are not the collection's numbers — and a standing collection is drawn
    // from by number.
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "March Book", "standing").await;
    for title in ["A March", "B March", "C March", "D March", "E March"] {
        let arr = seed_arrangement(&ctx.state, fx.org_id, title).await;
        add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
    }
    let live = items(&ctx, coll).await;
    assert_eq!(
        live.iter().map(|i| i.item.position).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );

    remove_piece(&ctx, &browser, fx.org_id, coll, live[2].item.id).await;

    let after = items(&ctx, coll).await;
    assert_eq!(
        after
            .iter()
            .map(|i| (i.item.position, i.arrangement_title.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (1, "A March"),
            (2, "B March"),
            (3, "D March"),
            (4, "E March"),
        ],
        "the pieces after the removed one shift down; no gap is left behind"
    );
}

#[tokio::test]
async fn restoring_a_piece_puts_it_back_where_it_was() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "March Book", "standing").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    for title in ["A March", "B March", "C March", "D March"] {
        let arr = seed_arrangement(&ctx.state, fx.org_id, title).await;
        add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
    }
    let live = items(&ctx, coll).await;
    let second = live[1].item.id;

    remove_piece(&ctx, &browser, fx.org_id, coll, second).await;
    assert_eq!(
        items(&ctx, coll)
            .await
            .iter()
            .map(|i| i.arrangement_title.as_str())
            .collect::<Vec<_>>(),
        vec!["A March", "C March", "D March"]
    );

    // Restoring is undoing the removal: it goes back to number 2 and pushes the
    // rest up, rather than landing at the end.
    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/{second}/undelete"),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);

    let after = items(&ctx, coll).await;
    assert_eq!(
        after
            .iter()
            .map(|i| (i.item.position, i.arrangement_title.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (1, "A March"),
            (2, "B March"),
            (3, "C March"),
            (4, "D March"),
        ],
        "the piece is back at its old number with the others shifted up"
    );
}

#[tokio::test]
async fn a_restore_into_a_shrunken_collection_lands_at_the_end() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "March Book", "standing").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    for title in ["A March", "B March", "C March"] {
        let arr = seed_arrangement(&ctx.state, fx.org_id, title).await;
        add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
    }
    let live = items(&ctx, coll).await;

    // Remove the third, then the others: its old number no longer exists.
    remove_piece(&ctx, &browser, fx.org_id, coll, live[2].item.id).await;
    remove_piece(&ctx, &browser, fx.org_id, coll, live[1].item.id).await;
    assert_eq!(items(&ctx, coll).await.len(), 1);

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    let token = form_csrf(&html);
    post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/{}/undelete", live[2].item.id),
        &[],
        &token,
    )
    .await;

    let after = items(&ctx, coll).await;
    assert_eq!(
        after
            .iter()
            .map(|i| (i.item.position, i.arrangement_title.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "A March"), (2, "C March")],
        "a number past the end clamps to the end rather than leaving a gap"
    );
}

#[tokio::test]
async fn a_piece_cannot_be_in_a_collection_twice_however_it_gets_there() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "March Book", "standing").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    let arr = seed_arrangement(&ctx.state, fx.org_id, "A March").await;
    let other = seed_arrangement(&ctx.state, fx.org_id, "B March").await;
    add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
    add_piece(&ctx, &browser, fx.org_id, coll, other).await;
    let live = items(&ctx, coll).await;
    let first = live[0].item.id;

    // The dropdown hides an arrangement that is already in the collection, but
    // the route must refuse a crafted post too.
    let token = session_csrf(&ctx.app, &browser, &detail_url).await;
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items"),
        &[("arrangement_id", &arr.to_string())],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::CONFLICT,
        "adding a piece already in the collection is refused"
    );
    assert_eq!(items(&ctx, coll).await.len(), 2);

    // Remove it, add it back, and the removed row must not be restorable — that
    // would list the same music under two numbers.
    remove_piece(&ctx, &browser, fx.org_id, coll, first).await;
    add_piece(&ctx, &browser, fx.org_id, coll, arr).await;

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    assert!(
        !html.contains(&format!("{first}/undelete")),
        "a removed piece whose arrangement is back is not offered for restore"
    );

    let token = session_csrf(&ctx.app, &browser, &detail_url).await;
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/{first}/undelete"),
        &[],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::CONFLICT,
        "…and the route refuses it even though the button is gone"
    );
    let after = items(&ctx, coll).await;
    assert_eq!(after.len(), 2, "the collection still holds two pieces");
    assert_eq!(
        after
            .iter()
            .filter(|i| i.item.arrangement_id == arr)
            .count(),
        1,
        "exactly one copy of the piece"
    );
}

#[tokio::test]
async fn the_removed_list_shows_one_restorable_entry_per_piece() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = create_collection(&ctx, &browser, fx.org_id, "March Book", "standing").await;
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    let arr = seed_arrangement(&ctx.state, fx.org_id, "A March").await;

    // Add and remove the same piece twice: two soft-deleted rows, one piece.
    for _ in 0..2 {
        add_piece(&ctx, &browser, fx.org_id, coll, arr).await;
        let item = items(&ctx, coll).await[0].item.id;
        remove_piece(&ctx, &browser, fx.org_id, coll, item).await;
    }

    let (rows, total) =
        lied::domain::collection_item::list_deleted_for_collection(&ctx.pool, coll, 50, 0)
            .await
            .unwrap();
    assert_eq!(total, 1, "one restorable entry, not one per removal");
    assert_eq!(rows.len(), 1);

    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    assert_eq!(
        html.matches("/undelete").count(),
        1,
        "the screen offers exactly one Restore for that piece"
    );

    // Restoring the listed one works and brings the piece back once.
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{detail_url}/items/{}/undelete", rows[0].item.id),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(items(&ctx, coll).await.len(), 1);
}

// ---------------------------------------------------------------------------
// Bulk assignment: one player on one part across the collection (issue #55)
// ---------------------------------------------------------------------------

async fn seed_part(
    state: &AppState,
    arr_id: Uuid,
    name: &str,
    instrument_key: &str,
    part_number: Option<i16>,
) -> Uuid {
    let instrument_id = sqlx::query_scalar!(
        r#"SELECT id FROM instrument WHERE key = $1"#,
        instrument_key
    )
    .fetch_one(&state.db)
    .await
    .expect("instrument");
    let id = Uuid::now_v7();
    lied::domain::voice::create(
        &state.db,
        id,
        arr_id,
        name,
        &lied::domain::voice::slugify(name),
        &[instrument_id],
        lied::domain::voice::NewPartNumber::Set(part_number),
        None,
    )
    .await
    .expect("voice");
    id
}

/// The value of the hidden input `name` on a page.
fn hidden_value(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = html
        .find(&marker)
        .unwrap_or_else(|| panic!("no hidden {name}"))
        + marker.len();
    html[start..].split('"').next().unwrap().to_string()
}

async fn trumpet_id(ctx: &Ctx) -> Uuid {
    sqlx::query_scalar!("SELECT id FROM instrument WHERE key = 'trumpet_bb'")
        .fetch_one(&ctx.pool)
        .await
        .unwrap()
}

/// The `pair` values a bulk-assign confirm form carries.
fn confirm_pairs(html: &str) -> Vec<String> {
    let marker = "name=\"pair\" value=\"";
    html.match_indices(marker)
        .map(|(at, _)| {
            html[at + marker.len()..]
                .split('"')
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// Three-piece march book: Trumpet 1 in pieces 1 and 3, piece 2 has only a
/// Trumpet 2. Returns (collection, item ids, piece-1 and piece-3 Trumpet 1).
async fn seed_march_book(
    ctx: &Ctx,
    browser: &Browser,
    org_id: Uuid,
) -> (Uuid, Vec<Uuid>, Uuid, Uuid) {
    let coll = create_collection(ctx, browser, org_id, "March Book", "standing").await;
    let a = seed_arrangement(&ctx.state, org_id, "Radetzky").await;
    let b = seed_arrangement(&ctx.state, org_id, "Florentiner").await;
    let c = seed_arrangement(&ctx.state, org_id, "Alte Kameraden").await;
    let a1 = seed_part(&ctx.state, a, "Trumpet in Bb 1", "trumpet_bb", Some(1)).await;
    seed_part(&ctx.state, b, "Trumpet in Bb 2", "trumpet_bb", Some(2)).await;
    let c1 = seed_part(&ctx.state, c, "Trompete 1", "trumpet_bb", Some(1)).await;
    for arr in [a, b, c] {
        add_piece(ctx, browser, org_id, coll, arr).await;
    }
    let item_ids = items(ctx, coll).await.iter().map(|i| i.item.id).collect();
    (coll, item_ids, a1, c1)
}

#[tokio::test]
async fn a_player_is_put_on_one_part_across_the_collection_after_a_preview() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item_ids, a1, c1) = seed_march_book(&ctx, &browser, fx.org_id).await;
    // Spaces and an umlaut: the redirect back must encode the username.
    let jorg = seed_user(&ctx.state, "jörg first").await;
    let page = format!("/admin/orgs/{}/collections/{coll}/bulk-assign", fx.org_id);

    // The collection's screens link here.
    let (_, detail) = load_page(
        &ctx.app,
        &browser,
        &format!("/admin/orgs/{}/collections/{coll}", fx.org_id),
    )
    .await;
    assert!(
        detail.contains("/bulk-assign"),
        "linked from the collection"
    );

    // The picker offers the parts that exist, with how widely.
    let (status, html) = load_page(&ctx.app, &browser, &page).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        html.contains("Trumpet in B♭ — part 1 (in 2 pieces)"),
        "{html}"
    );
    assert!(html.contains("Trumpet in B♭ — part 2 (in 1 piece)"));
    assert!(
        confirm_pairs(&html).is_empty(),
        "no confirm before a preview"
    );

    // Preview: two pieces to assign, one without the part. Nothing written.
    let trumpet: Uuid = sqlx::query_scalar!("SELECT id FROM instrument WHERE key = 'trumpet_bb'")
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    let preview = format!(
        "{page}?part={trumpet}%3A1&username={}",
        form_urlencoded("", "jörg first").trim_start_matches('=')
    );
    let (status, html) = load_page(&ctx.app, &browser, &preview).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{html}");
    assert_eq!(html.matches("will assign").count(), 2);
    assert!(html.contains("no such part in this piece"));
    assert!(html.contains("Trumpet in Bb 2"), "the near miss is shown");
    let pairs = confirm_pairs(&html);
    assert_eq!(
        pairs,
        vec![
            format!("{}:{a1}", item_ids[0]),
            format!("{}:{c1}", item_ids[2])
        ]
    );
    assert!(assignment_for(&ctx, item_ids[0], a1).await.is_none());

    // Confirm.
    let token = form_csrf(&html);
    // Post exactly what the rendered confirm form carries.
    let user_value = hidden_value(&html, "user_id");
    let part_value = hidden_value(&html, "part");
    assert_eq!(part_value, format!("{trumpet}:1"));
    assert_eq!(
        user_value,
        jorg.to_string(),
        "the confirm carries the previewed id"
    );
    let mut fields: Vec<(&str, &str)> = vec![("user_id", &user_value), ("part", &part_value)];
    for pair in &pairs {
        fields.push(("pair", pair));
    }
    let response = post_form(&ctx.app, &browser, &page, &fields, &token).await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        location.contains("username=j%C3%B6rg%20first"),
        "{location}"
    );
    assert!(assignment_for(&ctx, item_ids[0], a1).await.is_some());
    assert!(assignment_for(&ctx, item_ids[2], c1).await.is_some());
    assert_eq!(audit_count(&ctx.pool, "part_assignment.create").await, 2);

    // Back on the preview, the work reads as done.
    let (status, html) = load_page(&ctx.app, &browser, &location).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(html.matches("already plays").count(), 2);
    assert!(html.contains("Nothing to assign."));
}

#[tokio::test]
async fn a_stale_bulk_confirm_is_refused_and_nothing_is_assigned() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item_ids, a1, c1) = seed_march_book(&ctx, &browser, fx.org_id).await;
    let player = seed_user(&ctx.state, "player").await.to_string();
    let part = format!("{}:1", trumpet_id(&ctx).await);
    let page = format!("/admin/orgs/{}/collections/{coll}/bulk-assign", fx.org_id);
    let token = session_csrf(&ctx.app, &browser, &page).await;

    // Piece 3's part is deleted after the preview was rendered.
    sqlx::query!("UPDATE voice SET deleted_at = now() WHERE id = $1", c1)
        .execute(&ctx.pool)
        .await
        .unwrap();
    let first = format!("{}:{a1}", item_ids[0]);
    let third = format!("{}:{c1}", item_ids[2]);
    let response = post_form(
        &ctx.app,
        &browser,
        &page,
        &[
            ("user_id", &player),
            ("part", &part),
            ("pair", &first),
            ("pair", &third),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert!(body_text(response).await.contains("preview again"));
    assert!(
        assignment_for(&ctx, item_ids[0], a1).await.is_none(),
        "all or nothing"
    );
}

#[tokio::test]
async fn bulk_assignment_is_for_part_assigners_only() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let archivist = Browser::login(&ctx.app, "arch").await;
    let (coll, item_ids, a1, _) = seed_march_book(&ctx, &archivist, fx.org_id).await;
    let player = seed_user(&ctx.state, "player").await.to_string();
    let part = format!("{}:1", trumpet_id(&ctx).await);
    let page = format!("/admin/orgs/{}/collections/{coll}/bulk-assign", fx.org_id);
    let pair = format!("{}:{a1}", item_ids[0]);

    let musician = Browser::login(&ctx.app, "mus").await;
    let (status, _) = load_page(&ctx.app, &musician, &page).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let token = session_csrf(&ctx.app, &musician, "/admin").await;
    let response = post_form(
        &ctx.app,
        &musician,
        &page,
        &[("user_id", &player), ("part", &part), ("pair", &pair)],
        &token,
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "refused with the musician's own valid CSRF token"
    );
    assert!(assignment_for(&ctx, item_ids[0], a1).await.is_none());

    // A plain musician is already stopped at the console door, so the check
    // above would pass even if this screen forgot its own gate. A principal
    // gets INTO the console (for coverage) and is the case that pins it.
    let trumpet: Uuid = sqlx::query_scalar!("SELECT id FROM instrument WHERE key = 'trumpet_bb'")
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    let principal = seed_user(&ctx.state, "principal").await;
    membership::create(
        &ctx.state.db,
        Uuid::now_v7(),
        principal,
        fx.org_id,
        membership::MembershipFields {
            role: membership::Role::Musician,
            instrument_ids: &[trumpet],
            is_principal: true,
            principal_instrument_ids: &[trumpet],
        },
        None,
    )
    .await
    .expect("principal membership");
    let principal = Browser::login(&ctx.app, "principal").await;
    let (status, _) = load_page(&ctx.app, &principal, &page).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let token = session_csrf(&ctx.app, &principal, &page).await;
    let response = post_form(
        &ctx.app,
        &principal,
        &page,
        &[("user_id", &player), ("part", &part), ("pair", &pair)],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(assignment_for(&ctx, item_ids[0], a1).await.is_none());

    let conductor = Browser::login(&ctx.app, "cond").await;
    let token = session_csrf(&ctx.app, &conductor, &page).await;
    let response = post_form(
        &ctx.app,
        &conductor,
        &page,
        &[("user_id", &player), ("part", &part), ("pair", &pair)],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(assignment_for(&ctx, item_ids[0], a1).await.is_some());
}

/// Review of #55, finding 5: the confirm carries the previewed player's id,
/// not the username. A rename between preview and confirm, with someone else
/// taking the old name, must not hand the parts to that someone else.
#[tokio::test]
async fn the_confirm_assigns_the_previewed_player_even_after_a_rename() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item_ids, a1, c1) = seed_march_book(&ctx, &browser, fx.org_id).await;
    let anna = seed_user(&ctx.state, "anna").await;
    let page = format!("/admin/orgs/{}/collections/{coll}/bulk-assign", fx.org_id);
    let trumpet = trumpet_id(&ctx).await;
    let (_, html) = load_page(
        &ctx.app,
        &browser,
        &format!("{page}?part={trumpet}%3A1&username=anna"),
    )
    .await;
    let token = form_csrf(&html);
    let user_value = hidden_value(&html, "user_id");
    let part_value = hidden_value(&html, "part");
    let pairs = confirm_pairs(&html);

    // Anna becomes "anna-k"; a different person signs up as "anna".
    sqlx::query!(
        r#"UPDATE "user" SET username = 'anna-k' WHERE id = $1"#,
        anna
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    // Slugs never change on rename, so the newcomer gets a slug of their own.
    let impostor = Uuid::now_v7();
    user::create(
        &ctx.state.db,
        impostor,
        "anna-2",
        "anna",
        None,
        None,
        "Another Anna",
        false,
        None,
    )
    .await
    .expect("impostor");

    let mut fields: Vec<(&str, &str)> = vec![("user_id", &user_value), ("part", &part_value)];
    for pair in &pairs {
        fields.push(("pair", pair));
    }
    let response = post_form(&ctx.app, &browser, &page, &fields, &token).await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    for (item, voice) in [(item_ids[0], a1), (item_ids[2], c1)] {
        let players: Vec<Uuid> = players_for(&ctx, item, voice)
            .await
            .into_iter()
            .map(|a| a.user_id)
            .collect();
        assert_eq!(
            players,
            vec![anna],
            "the previewed person, not the new holder of the name"
        );
    }
    assert!(!players_for(&ctx, item_ids[0], a1)
        .await
        .iter()
        .any(|a| a.user_id == impostor));
    // The redirect shows the preview under Anna's current name.
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(location.contains("username=anna-k"), "{location}");
}

/// Review of #55, finding 3 through the console: a voice renumbered after the
/// preview makes the open confirm form stale.
#[tokio::test]
async fn a_console_confirm_for_a_renumbered_voice_is_refused() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let (coll, item_ids, a1, c1) = seed_march_book(&ctx, &browser, fx.org_id).await;
    let player = seed_user(&ctx.state, "player").await.to_string();
    let part = format!("{}:1", trumpet_id(&ctx).await);
    let page = format!("/admin/orgs/{}/collections/{coll}/bulk-assign", fx.org_id);
    let token = session_csrf(&ctx.app, &browser, &page).await;

    sqlx::query!("UPDATE voice SET part_number = 2 WHERE id = $1", c1)
        .execute(&ctx.pool)
        .await
        .unwrap();
    let first = format!("{}:{a1}", item_ids[0]);
    let third = format!("{}:{c1}", item_ids[2]);
    let response = post_form(
        &ctx.app,
        &browser,
        &page,
        &[
            ("user_id", &player),
            ("part", &part),
            ("pair", &first),
            ("pair", &third),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert!(assignment_for(&ctx, item_ids[0], a1).await.is_none());
}

#[tokio::test]
async fn a_voice_part_number_is_set_from_the_catalog_screens() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let voices_url = format!("/admin/orgs/{}/arrangements/{arr}/voices", fx.org_id);
    let trumpet: Uuid = sqlx::query_scalar!("SELECT id FROM instrument WHERE key = 'trumpet_bb'")
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    let (_, html) = load_page(&ctx.app, &browser, &voices_url).await;
    assert!(html.contains("name=\"part_number\""));
    let token = form_csrf(&html);
    let trumpet_str = trumpet.to_string();

    let response = post_form(
        &ctx.app,
        &browser,
        &voices_url,
        &[
            ("name", "Trumpet in Bb 2"),
            ("instrument_id", &trumpet_str),
            ("part_number", "2"),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let stored: Option<i16> = sqlx::query_scalar!(
        "SELECT part_number FROM voice WHERE arrangement_id = $1",
        arr
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(stored, Some(2));

    // Zero is refused; blank means "no number".
    let response = post_form(
        &ctx.app,
        &browser,
        &voices_url,
        &[
            ("name", "Trumpet 0"),
            ("instrument_id", &trumpet_str),
            ("part_number", "0"),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    let response = post_form(
        &ctx.app,
        &browser,
        &voices_url,
        &[
            ("name", "Trumpet"),
            ("instrument_id", &trumpet_str),
            ("part_number", ""),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let unnumbered: Option<i16> = sqlx::query_scalar!(
        "SELECT part_number FROM voice WHERE arrangement_id = $1 AND name = 'Trumpet'",
        arr
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(unnumbered, None);

    // Review of #55, finding 7: a blank number on the ADD form takes it from
    // the name, so new voices number like the backfilled ones.
    let response = post_form(
        &ctx.app,
        &browser,
        &voices_url,
        &[
            ("name", "Trumpet in Bb 3"),
            ("instrument_id", &trumpet_str),
            ("part_number", ""),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let (voice_id, guessed): (Uuid, Option<i16>) = sqlx::query_as(
        "SELECT id, part_number FROM voice WHERE arrangement_id = $1 AND name = 'Trumpet in Bb 3'",
    )
    .bind(arr)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(guessed, Some(3));

    // On the EDIT form, blank means "no number" — the field shows the current
    // value, so clearing it is deliberate.
    let edit_url = format!("{voices_url}/{voice_id}");
    let (_, html) = load_page(&ctx.app, &browser, &edit_url).await;
    let version = hidden_value(&html, "expected_version");
    let response = post_form(
        &ctx.app,
        &browser,
        &edit_url,
        &[
            ("expected_version", &version),
            ("name", "Trumpet in Bb 3"),
            ("instrument_id", &trumpet_str),
            ("part_number", ""),
        ],
        &form_csrf(&html),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let cleared: Option<i16> = sqlx::query_scalar("SELECT part_number FROM voice WHERE id = $1")
        .bind(voice_id)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    assert_eq!(cleared, None);
}

#[tokio::test]
async fn the_console_restores_a_tag_only_through_its_own_org() {
    use lied::domain::tag;
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
    let ours = tag::create(&ctx.pool, Uuid::now_v7(), fx.org_id, "Festive", None, None)
        .await
        .expect("our tag");
    let theirs = tag::create(&ctx.pool, Uuid::now_v7(), other_org, "Theirs", None, None)
        .await
        .expect("their tag");
    for id in [ours.id, theirs.id] {
        assert!(tag::soft_delete(&ctx.pool, id, None).await.unwrap());
    }

    let tags_url = format!("/admin/orgs/{}/tags", fx.org_id);
    let token = session_csrf(&ctx.app, &browser, &tags_url).await;

    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{tags_url}/{}/undelete", theirs.id),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    assert!(
        tag::find_by_id(&ctx.pool, theirs.id)
            .await
            .unwrap()
            .is_none(),
        "another org's tag stays deleted"
    );

    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{tags_url}/{}/undelete", ours.id),
        &[],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(tag::find_by_id(&ctx.pool, ours.id).await.unwrap().is_some());
    assert_eq!(audit_count(&ctx.pool, "tag.undelete").await, 1);
}

// ---------------------------------------------------------------------------
// Tests — #104: standing collections by label, gaps, sections, Renumber
// ---------------------------------------------------------------------------

/// Post a form to a collection's sub-path, fetching a fresh CSRF token from
/// its detail page first.
async fn post_to_collection(
    ctx: &Ctx,
    browser: &Browser,
    org_id: Uuid,
    coll: Uuid,
    sub_path: &str,
    fields: &[(&str, &str)],
) -> axum::response::Response {
    let detail_url = format!("/admin/orgs/{org_id}/collections/{coll}");
    let (_, html) = load_page(&ctx.app, browser, &detail_url).await;
    let token = form_csrf(&html);
    post_form(
        &ctx.app,
        browser,
        &format!("{detail_url}{sub_path}"),
        fields,
        &token,
    )
    .await
}

/// Add `title` to a collection under the given form fields; returns the
/// response status.
async fn add_with(
    ctx: &Ctx,
    browser: &Browser,
    org_id: Uuid,
    coll: Uuid,
    title: &str,
    fields: &[(&str, &str)],
) -> (axum::http::StatusCode, String) {
    let arr = seed_arrangement(&ctx.state, org_id, title).await;
    let arr = arr.to_string();
    let mut all = vec![("arrangement_id", arr.as_str())];
    all.extend_from_slice(fields);
    let response = post_to_collection(ctx, browser, org_id, coll, "/items", &all).await;
    let status = response.status();
    (status, body_text(response).await)
}

fn labels(views: &[lied::domain::collection_item::CollectionItemView]) -> Vec<String> {
    views.iter().map(|v| v.item.label.clone()).collect()
}

/// A standing collection with pieces under the given labels, added through
/// the console (each label in turn is the suggestion, a gap or a letter).
async fn standing_with(ctx: &Ctx, browser: &Browser, org_id: Uuid, wanted: &[&str]) -> Uuid {
    let coll = create_collection(ctx, browser, org_id, "Marschbuch", "standing").await;
    for (i, label) in wanted.iter().enumerate() {
        let (status, body) = add_with(
            ctx,
            browser,
            org_id,
            coll,
            &format!("March {i}"),
            &[("label", *label)],
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::SEE_OTHER, "{label}: {body}");
    }
    coll
}

#[tokio::test]
async fn a_standing_piece_is_added_by_suggestion_gap_or_letter_label() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = standing_with(&ctx, &browser, fx.org_id, &["1", "2", "3"]).await;
    let live = items(&ctx, coll).await;
    remove_piece(&ctx, &browser, fx.org_id, coll, live[1].item.id).await;

    // The page offers the suggestion (preselected) and the gap.
    let detail_url = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    let (_, html) = load_page(&ctx.app, &browser, &detail_url).await;
    assert!(
        html.contains("<option value=\"4\" selected>4 (next)"),
        "{html}"
    );
    assert!(html.contains("<option value=\"2\">2 (free)"), "{html}");

    // The gap is taken as chosen.
    let (status, _) = add_with(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        "Gap piece",
        &[("label", "2")],
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);

    // The letter field wins over the select.
    let (status, _) = add_with(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        "Slipped in",
        &[("label", "4"), ("letter_label", "3A")],
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);
    assert_eq!(labels(&items(&ctx, coll).await), vec!["1", "2", "3", "3a"]);

    // A number far past the end is refused with the domain's reason.
    let (status, body) =
        add_with(&ctx, &browser, fx.org_id, coll, "Typo", &[("label", "40")]).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(body.contains("Label 40 is not allowed"), "{body}");
    // A taken one is a conflict.
    let (status, body) = add_with(&ctx, &browser, fx.org_id, coll, "Twin", &[("label", "3")]).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert!(body.contains("another piece has it"), "{body}");
}

#[tokio::test]
async fn a_standing_piece_is_relabelled_or_swapped_from_its_row() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = standing_with(&ctx, &browser, fx.org_id, &["1", "2", "3"]).await;
    let before = items(&ctx, coll).await;
    remove_piece(&ctx, &browser, fx.org_id, coll, before[1].item.id).await;
    // Fresh versions: the removal moved piece 3's position (not its label).
    let live = items(&ctx, coll).await;
    let (first, third) = (&live[0].item, &live[1].item);
    assert_eq!(third.label, "3");

    // A stale row form is refused (someone moved the piece meanwhile).
    let stale = (first.updated_at.timestamp_millis() - 1).to_string();
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        &format!("/items/{}/relabel", first.id),
        &[("label", "2"), ("expected_version", &stale)],
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );

    // Piece 3 moves into the gap.
    let version = third.updated_at.timestamp_millis().to_string();
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        &format!("/items/{}/relabel", third.id),
        &[("label", "2"), ("expected_version", &version)],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(labels(&items(&ctx, coll).await), vec!["1", "2"]);

    // And swaps with piece 1.
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        &format!("/items/{}/swap", third.id),
        &[("with_item_id", &first.id.to_string())],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let after = items(&ctx, coll).await;
    assert_eq!(
        after[0].item.id, third.id,
        "the swapped piece is now number 1"
    );
    assert_eq!(after[0].item.label, "1");
    assert_eq!(audit_count(&ctx.pool, "collection_item.swap").await, 1);
    assert_eq!(audit_count(&ctx.pool, "collection_item.update").await, 1);
}

#[tokio::test]
async fn gaps_render_as_free_rows_and_open_numbers_as_one_line() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = standing_with(&ctx, &browser, fx.org_id, &["1", "2", "3", "4"]).await;
    let live = items(&ctx, coll).await;
    remove_piece(&ctx, &browser, fx.org_id, coll, live[2].item.id).await;
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        "/sections",
        &[("name", "Polkas"), ("starts_at", "61")],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let (status, _) = add_with(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        "First polka",
        &[("label", "61")],
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);

    let (_, html) = load_page(
        &ctx.app,
        &browser,
        &format!("/admin/orgs/{}/collections/{coll}", fx.org_id),
    )
    .await;
    assert!(
        html.contains("<td>3</td><td colspan=\"4\">free</td>"),
        "the removed piece's number is a free row: {html}"
    );
    assert!(
        html.contains("5–60 free"),
        "the open numbers before the next section are one line: {html}"
    );
    assert!(html.contains("Polkas"), "{html}");
    // The add select groups the choices per range.
    assert!(
        html.contains("<optgroup label=\"Polkas (from 61)\">"),
        "{html}"
    );
    assert!(
        html.contains("<option value=\"62\" selected>62 (next)"),
        "{html}"
    );
}

/// The preview token a renumber page carries.
fn preview_token(html: &str) -> String {
    let marker = "name=\"preview_token\" value=\"";
    let start = html.find(marker).expect("preview carries a token") + marker.len();
    html[start..].split('"').next().unwrap().to_string()
}

#[tokio::test]
async fn renumber_previews_only_the_changes_and_refuses_a_stale_preview() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = standing_with(&ctx, &browser, fx.org_id, &["1", "2", "3"]).await;
    let live = items(&ctx, coll).await;
    remove_piece(&ctx, &browser, fx.org_id, coll, live[1].item.id).await;
    let renumber_url = format!("/admin/orgs/{}/collections/{coll}/renumber", fx.org_id);

    let (status, html) = load_page(&ctx.app, &browser, &renumber_url).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("re-sleeved"), "{html}");
    assert!(
        html.contains("<td>March 2</td><td>3</td><td>2</td>"),
        "piece 3 becomes 2: {html}"
    );
    assert!(
        !html.contains("March 0"),
        "an unchanged piece is not listed: {html}"
    );
    let token = preview_token(&html);

    // A change after the preview: the confirm re-renders the new preview.
    let (status, _) = add_with(&ctx, &browser, fx.org_id, coll, "Late", &[("label", "4")]).await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);
    let csrf = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &renumber_url,
        &[("preview_token", &token)],
        &csrf,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    let html = body_text(response).await;
    assert!(
        html.contains("changed since you opened the preview"),
        "{html}"
    );
    assert_eq!(labels(&items(&ctx, coll).await), vec!["1", "3", "4"]);

    // Confirming the fresh preview applies it.
    let token = preview_token(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &renumber_url,
        &[("preview_token", &token)],
        &csrf,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(labels(&items(&ctx, coll).await), vec!["1", "2", "3"]);
    assert_eq!(audit_count(&ctx.pool, "collection.renumber").await, 1);
}

#[tokio::test]
async fn restoring_a_piece_whose_number_was_taken_asks_for_one() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = standing_with(&ctx, &browser, fx.org_id, &["1", "2", "3"]).await;
    let removed_id = items(&ctx, coll).await[1].item.id;
    remove_piece(&ctx, &browser, fx.org_id, coll, removed_id).await;
    let (status, _) = add_with(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        "Newcomer",
        &[("label", "2")],
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);

    let (_, html) = load_page(
        &ctx.app,
        &browser,
        &format!("/admin/orgs/{}/collections/{coll}", fx.org_id),
    )
    .await;
    assert!(html.contains("Its number is taken"), "{html}");

    // Without a number: refused with the domain's reason.
    let undelete = format!("/items/{removed_id}/undelete");
    let response = post_to_collection(&ctx, &browser, fx.org_id, coll, &undelete, &[]).await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert!(body_text(response).await.contains("is no longer free"));

    // With a letter label: restored there.
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        &undelete,
        &[("label", ""), ("letter_label", "2a")],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(labels(&items(&ctx, coll).await), vec!["1", "2", "2a", "3"]);
}

#[tokio::test]
async fn sections_are_added_moved_and_deleted_with_their_version() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "cond").await;
    let coll = standing_with(&ctx, &browser, fx.org_id, &["1"]).await;
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        "/sections",
        &[("name", "Polkas"), ("starts_at", "61")],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let section = lied::domain::collection_section::list_for_collection(&ctx.pool, coll)
        .await
        .unwrap()
        .remove(0);

    // Two sections cannot share a start.
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        "/sections",
        &[("name", "Waltzes"), ("starts_at", "61")],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);

    // A stale edit is refused; a current one moves the start.
    let path = format!("/sections/{}", section.id);
    let stale = (section.updated_at.timestamp_millis() - 1).to_string();
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        &path,
        &[
            ("name", "Polkas"),
            ("starts_at", "51"),
            ("expected_version", &stale),
        ],
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );
    let version = section.updated_at.timestamp_millis().to_string();
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        &path,
        &[
            ("name", "Polkas & Co"),
            ("starts_at", "51"),
            ("expected_version", &version),
        ],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let moved = lied::domain::collection_section::find_by_id(&ctx.pool, section.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((moved.name.as_str(), moved.starts_at), ("Polkas & Co", 51));

    // Delete needs the current version too.
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        &format!("{path}/delete"),
        &[("expected_version", &version)],
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );
    let version = moved.updated_at.timestamp_millis().to_string();
    let response = post_to_collection(
        &ctx,
        &browser,
        fx.org_id,
        coll,
        &format!("{path}/delete"),
        &[("expected_version", &version)],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(
        lied::domain::collection_section::list_for_collection(&ctx.pool, coll)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(audit_count(&ctx.pool, "collection_section.create").await, 1);
    assert_eq!(audit_count(&ctx.pool, "collection_section.update").await, 1);
    assert_eq!(audit_count(&ctx.pool, "collection_section.delete").await, 1);

    // A program has no sections UI, and its Remove buttons sit in the rows
    // (pointing at forms outside the reorder form).
    let program = create_collection(&ctx, &browser, fx.org_id, "Spring Concert", "program").await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Overture").await;
    add_piece(&ctx, &browser, fx.org_id, program, arr).await;
    let (_, html) = load_page(
        &ctx.app,
        &browser,
        &format!("/admin/orgs/{}/collections/{program}", fx.org_id),
    )
    .await;
    assert!(!html.contains("Add section"), "{html}");
    let item = items(&ctx, program).await[0].item.id;
    assert!(
        html.contains(&format!("form=\"remove-{item}\""))
            && html.contains(&format!("id=\"remove-{item}\"")),
        "{html}"
    );
}

#[tokio::test]
async fn a_musician_reaches_none_of_the_numbering_screens() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let arch = Browser::login(&ctx.app, "arch").await;
    let coll = standing_with(&ctx, &arch, fx.org_id, &["1"]).await;
    let item = items(&ctx, coll).await[0].item.id;

    let musician = Browser::login(&ctx.app, "mus").await;
    let base = format!("/admin/orgs/{}/collections/{coll}", fx.org_id);
    let (status, _) = load_page(&ctx.app, &musician, &format!("{base}/renumber")).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let token = session_csrf(&ctx.app, &musician, "/admin").await;
    for (path, fields) in [
        (
            format!("{base}/sections"),
            vec![("name", "X"), ("starts_at", "5")],
        ),
        (format!("{base}/items/{item}/relabel"), vec![("label", "2")]),
        (format!("{base}/renumber"), vec![("preview_token", "x")]),
    ] {
        let response = post_form(&ctx.app, &musician, &path, &fields, &token).await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    assert_eq!(labels(&items(&ctx, coll).await), vec!["1"]);
}

/// A piece's Move dropdown offers what *that piece* may take: its own number
/// does not count against it. Before, every row reused the add-piece
/// choices, so the highest piece was offered "highest + 1", which the
/// domain then refused (moving 3 to 4 in 1, 2, 3 would invent a gap).
#[tokio::test]
async fn the_move_dropdown_offers_only_numbers_the_piece_may_take() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let coll = standing_with(&ctx, &browser, fx.org_id, &["1", "2", "3"]).await;
    let live = items(&ctx, coll).await;
    let highest = live
        .iter()
        .find(|v| v.item.label == "3")
        .expect("piece 3")
        .item
        .id;
    let lowest = live
        .iter()
        .find(|v| v.item.label == "1")
        .expect("piece 1")
        .item
        .id;

    let (_, html) = load_page(
        &ctx.app,
        &browser,
        &format!("/admin/orgs/{}/collections/{coll}", fx.org_id),
    )
    .await;
    let move_form = |id: uuid::Uuid| -> String {
        let start = html
            .find(&format!("{id}/relabel"))
            .unwrap_or_else(|| panic!("a Move form for {id}: {html}"));
        let end = start + html[start..].find("</form>").expect("form end");
        html[start..end].to_string()
    };
    assert!(
        !move_form(highest).contains("value=\"4\""),
        "piece 3 may not move to 4: {}",
        move_form(highest)
    );
    assert!(
        move_form(lowest).contains("value=\"4\""),
        "piece 1 may move to the end: {}",
        move_form(lowest)
    );
}

/// #57: the console voice form takes a primary instrument and "also plays"
/// extras; the list shows them joined, the edit form preselects the extras,
/// and saving without extras drops them.
#[tokio::test]
async fn a_console_voice_carries_extra_instruments() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let browser = Browser::login(&ctx.app, "arch").await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let voices_url = format!("/admin/orgs/{}/arrangements/{arr}/voices", fx.org_id);
    let key = |k: &'static str| {
        let pool = ctx.pool.clone();
        async move {
            sqlx::query_scalar!("SELECT id FROM instrument WHERE key = $1", k)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let flute: Uuid = key("flute").await;
    let piccolo: Uuid = key("piccolo").await;
    let stored = |voice: Uuid| {
        let pool = ctx.pool.clone();
        async move {
            sqlx::query_scalar!(
                "SELECT instrument_id FROM voice_instrument WHERE voice_id = $1 ORDER BY position",
                voice
            )
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };

    let (_, html) = load_page(&ctx.app, &browser, &voices_url).await;
    assert!(html.contains("name=\"also_instrument_ids\""), "{html}");
    let token = form_csrf(&html);
    let (flute_s, piccolo_s) = (flute.to_string(), piccolo.to_string());
    let response = post_form(
        &ctx.app,
        &browser,
        &voices_url,
        &[
            ("name", "Flute 2 / Piccolo"),
            ("instrument_id", &flute_s),
            ("also_instrument_ids", &piccolo_s),
            // Picking the primary again among the extras means the same.
            ("also_instrument_ids", &flute_s),
            ("part_number", "2"),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let voice: Uuid = sqlx::query_scalar!("SELECT id FROM voice WHERE arrangement_id = $1", arr)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    assert_eq!(stored(voice).await, vec![flute, piccolo]);

    let (_, html) = load_page(&ctx.app, &browser, &voices_url).await;
    assert!(html.contains("Flute / Piccolo"), "{html}");

    let edit_url = format!("{voices_url}/{voice}");
    let (_, html) = load_page(&ctx.app, &browser, &edit_url).await;
    assert!(
        html.contains(&format!("<option value=\"{piccolo}\" selected")),
        "the extra is preselected: {html}"
    );
    let version = html
        .split("name=\"expected_version\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("expected_version")
        .to_string();
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &browser,
        &edit_url,
        &[
            ("name", "Flute 2"),
            ("instrument_id", &flute_s),
            ("part_number", "2"),
            ("expected_version", &version),
        ],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(
        stored(voice).await,
        vec![flute],
        "no extras: only the primary"
    );
}
