//! Integration tests for issue #36 in the console: the Annotations block on
//! the arrangement screen (staff read, owner/conductor write), the edit page,
//! and the org-wide Annotations section — over real HTTP with a session cookie
//! and the CSRF contract the pages render. `/v1` behaviour is covered by
//! `tests/global_annotations.rs`.

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
    sqlx::query_scalar!(
        r#"SELECT count(*) as "n!" FROM audit_log WHERE action = $1"#,
        action
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Ids of the annotations on an arrangement, oldest first.
async fn annotation_ids(ctx: &Ctx, arr: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar!(
        "SELECT id FROM global_annotation WHERE arrangement_id = $1 ORDER BY created_at",
        arr
    )
    .fetch_all(&ctx.pool)
    .await
    .unwrap()
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

#[tokio::test]
async fn a_conductor_adds_edits_and_deletes_annotations_from_the_arrangement_screen() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let browser = Browser::login(&ctx.app, "cond").await;
    let page = format!("/admin/orgs/{}/arrangements/{arr}", fx.org_id);

    let (status, html) = load_page(&ctx.app, &browser, &page).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("No annotations yet."));
    assert!(html.contains("Add an annotation"));

    // Add.
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{page}/annotations"),
        &[
            ("type", "structure"),
            ("content", "Cut bars 40–56 & go to the <coda>"),
        ],
        &form_csrf(&html),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let ids = annotation_ids(&ctx, arr).await;
    assert_eq!(ids.len(), 1);
    let (_, html) = load_page(&ctx.app, &browser, &page).await;
    assert!(
        html.contains("Cut bars 40–56 &amp; go to the &lt;coda&gt;"),
        "rendered, and escaped"
    );
    assert!(html.contains("Structure (cuts, repeats)"));
    assert_eq!(audit_count(&ctx.pool, "global_annotation.create").await, 1);

    // Edit.
    let edit = format!("{page}/annotations/{}", ids[0]);
    let (status, html) = load_page(&ctx.app, &browser, &edit).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let version = hidden_value(&html, "expected_version");
    let response = post_form(
        &ctx.app,
        &browser,
        &edit,
        &[
            ("expected_version", &version),
            ("type", "tempo"),
            ("content", "Molto rit. into bar 56"),
        ],
        &form_csrf(&html),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let (_, html) = load_page(&ctx.app, &browser, &page).await;
    assert!(html.contains("Molto rit. into bar 56"));
    assert_eq!(audit_count(&ctx.pool, "global_annotation.update").await, 1);

    // The stale version from before the edit is refused.
    let (_, edit_html) = load_page(&ctx.app, &browser, &edit).await;
    let response = post_form(
        &ctx.app,
        &browser,
        &edit,
        &[
            ("expected_version", &version),
            ("type", "general"),
            ("content", "lost update"),
        ],
        &form_csrf(&edit_html),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{edit}/delete"),
        &[("expected_version", &version)],
        &form_csrf(&edit_html),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(annotation_ids(&ctx, arr).await.len(), 1);

    // Delete with the current version: gone for good, audited.
    let current = hidden_value(&edit_html, "expected_version");
    let response = post_form(
        &ctx.app,
        &browser,
        &format!("{edit}/delete"),
        &[("expected_version", &current)],
        &form_csrf(&edit_html),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(annotation_ids(&ctx, arr).await.is_empty());
    assert_eq!(audit_count(&ctx.pool, "global_annotation.delete").await, 1);
}

#[tokio::test]
async fn an_unknown_type_or_empty_annotation_is_refused() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let browser = Browser::login(&ctx.app, "cond").await;
    let page = format!("/admin/orgs/{}/arrangements/{arr}", fx.org_id);
    let (_, html) = load_page(&ctx.app, &browser, &page).await;
    let token = form_csrf(&html);
    for fields in [
        [("type", "cut"), ("content", "x")],
        [("type", "tempo"), ("content", "   ")],
    ] {
        let response = post_form(
            &ctx.app,
            &browser,
            &format!("{page}/annotations"),
            &fields,
            &token,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }
    assert!(annotation_ids(&ctx, arr).await.is_empty());
}

#[tokio::test]
async fn an_archivist_reads_annotations_but_cannot_write_them() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let conductor = Browser::login(&ctx.app, "cond").await;
    let page = format!("/admin/orgs/{}/arrangements/{arr}", fx.org_id);
    let (_, html) = load_page(&ctx.app, &conductor, &page).await;
    post_form(
        &ctx.app,
        &conductor,
        &format!("{page}/annotations"),
        &[("type", "dynamics"), ("content", "pp until bar 9")],
        &form_csrf(&html),
    )
    .await;
    let id = annotation_ids(&ctx, arr).await[0];

    let archivist = Browser::login(&ctx.app, "arch").await;
    let (status, html) = load_page(&ctx.app, &archivist, &page).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        html.contains("pp until bar 9"),
        "staff read the annotations"
    );
    assert!(!html.contains("Add an annotation"), "but get no add form");
    assert!(
        !html.contains(&format!("/annotations/{id}\"")),
        "and no edit link"
    );
    assert!(html.contains("Only the conductor or an owner"));

    // The routes hold the line too — with the archivist's own valid token, so
    // this pins the authorization check rather than CSRF.
    let token = form_csrf(&html);
    let response = post_form(
        &ctx.app,
        &archivist,
        &format!("{page}/annotations"),
        &[("type", "general"), ("content", "x")],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    let edit = format!("{page}/annotations/{id}");
    let (status, _) = load_page(&ctx.app, &archivist, &edit).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    let response = post_form(
        &ctx.app,
        &archivist,
        &format!("{edit}/delete"),
        &[("expected_version", "0")],
        &token,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(annotation_ids(&ctx, arr).await, vec![id]);
}

#[tokio::test]
async fn the_annotations_section_lists_the_orgs_notes_for_their_authors_only() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let bolero = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let egmont = seed_arrangement(&ctx.state, fx.org_id, "Egmont Overture").await;
    let conductor = Browser::login(&ctx.app, "cond").await;
    for (arr, content) in [(bolero, "snare stays pp"), (egmont, "fermata bar 3")] {
        let page = format!("/admin/orgs/{}/arrangements/{arr}", fx.org_id);
        let (_, html) = load_page(&ctx.app, &conductor, &page).await;
        post_form(
            &ctx.app,
            &conductor,
            &format!("{page}/annotations"),
            &[("type", "general"), ("content", content)],
            &form_csrf(&html),
        )
        .await;
    }
    // A removed arrangement's notes drop out of the overview with it.
    assert!(arrangement::soft_delete(&ctx.pool, egmont, None)
        .await
        .unwrap());

    let overview = format!("/admin/orgs/{}/annotations", fx.org_id);
    let (status, html) = load_page(&ctx.app, &conductor, &overview).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("snare stays pp"));
    assert!(html.contains("Bolero"));
    assert!(
        html.contains(&format!("/arrangements/{bolero}")),
        "linked to its piece"
    );
    assert!(!html.contains("fermata bar 3"));
    assert!(!html.contains("arrives in issue"), "the stub is gone");

    // The section follows the matrix: authors only. A plain musician is
    // already stopped at the console door; a principal gets in (for coverage)
    // and is the case that pins this section's own gate.
    let principal = seed_user(&ctx.state, "principal").await;
    membership::create(
        &ctx.state.db,
        Uuid::now_v7(),
        principal,
        fx.org_id,
        membership::MembershipFields {
            role: membership::Role::Musician,
            instrument_ids: &[],
            is_principal: true,
            principal_instrument_ids: &[],
        },
        None,
    )
    .await
    .expect("principal membership");
    for who in ["arch", "mus", "principal"] {
        let browser = Browser::login(&ctx.app, who).await;
        let (status, _) = load_page(&ctx.app, &browser, &overview).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{who}");
    }
}

// ── review of #36 ───────────────────────────────────────────────────────────

/// Finding 5: a browser submits textarea line breaks as CRLF. Saving a note
/// whose content was not touched must not rewrite its line endings, and
/// saving with no change at all writes nothing.
#[tokio::test]
async fn saving_an_untouched_multiline_note_does_not_rewrite_it() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let conductor_id = sqlx::query_scalar!(r#"SELECT id FROM "user" WHERE username = 'cond'"#)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    // A note with LF line endings, as a /v1 client would write it.
    let note = lied::domain::global_annotation::create(
        &ctx.pool,
        Uuid::now_v7(),
        arr,
        conductor_id,
        "tempo",
        "bar 12: rit.\nbar 40: a tempo",
    )
    .await
    .unwrap();
    let browser = Browser::login(&ctx.app, "cond").await;
    let edit = format!(
        "/admin/orgs/{}/arrangements/{arr}/annotations/{}",
        fx.org_id, note.id
    );
    let (_, html) = load_page(&ctx.app, &browser, &edit).await;
    let version = hidden_value(&html, "expected_version");

    // Same type, same content (as the browser sends it): nothing is written.
    let response = post_form(
        &ctx.app,
        &browser,
        &edit,
        &[
            ("expected_version", &version),
            ("type", "tempo"),
            ("content", "bar 12: rit.\r\nbar 40: a tempo"),
        ],
        &form_csrf(&html),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(audit_count(&ctx.pool, "global_annotation.update").await, 0);

    // Only the type changes: the content keeps its LF line endings.
    let response = post_form(
        &ctx.app,
        &browser,
        &edit,
        &[
            ("expected_version", &version),
            ("type", "dynamics"),
            ("content", "bar 12: rit.\r\nbar 40: a tempo"),
        ],
        &form_csrf(&html),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let stored = sqlx::query_scalar!(
        "SELECT content FROM global_annotation WHERE id = $1",
        note.id
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(stored, "bar 12: rit.\nbar 40: a tempo");

    // The edit page emits one newline after <textarea> for the parser to
    // drop, so content that itself starts with a newline survives a round trip.
    assert!(html.contains("<textarea name=\"content\" rows=\"4\" required>\nbar 12"));
}

/// Finding 6 and 15: past the page cap, the arrangement screen pages instead
/// of leaving notes with no Edit link anywhere in the console.
#[tokio::test]
async fn annotations_past_the_page_cap_are_paged_not_lost() {
    let ctx = Ctx::with_page_size(1).await;
    let fx = seed(&ctx.state).await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let browser = Browser::login(&ctx.app, "cond").await;
    let page = format!("/admin/orgs/{}/arrangements/{arr}", fx.org_id);
    for content in ["first note", "second note"] {
        let (_, html) = load_page(&ctx.app, &browser, &page).await;
        post_form(
            &ctx.app,
            &browser,
            &format!("{page}/annotations"),
            &[("type", "general"), ("content", content)],
            &form_csrf(&html),
        )
        .await;
    }
    let ids = annotation_ids(&ctx, arr).await;

    let (_, html) = load_page(&ctx.app, &browser, &page).await;
    assert!(html.contains("first note") && !html.contains("second note"));
    assert!(html.contains("Annotations 1–1 of 2."), "{html}");
    assert!(html.contains("?annotations_offset=1"), "a link to the rest");

    let (_, html) = load_page(&ctx.app, &browser, &format!("{page}?annotations_offset=1")).await;
    assert!(html.contains("second note") && !html.contains("first note"));
    assert!(
        html.contains(&format!("/annotations/{}\"", ids[1])),
        "the second note has its Edit link"
    );
    assert!(html.contains("?annotations_offset=0"), "and a way back");
}

/// Finding 14: the server does not present a UTC instant as a bare local-
/// looking date; it says it is UTC and carries the machine-readable instant.
#[tokio::test]
async fn change_times_are_marked_as_utc() {
    let ctx = Ctx::new().await;
    let fx = seed(&ctx.state).await;
    let arr = seed_arrangement(&ctx.state, fx.org_id, "Bolero").await;
    let browser = Browser::login(&ctx.app, "cond").await;
    let page = format!("/admin/orgs/{}/arrangements/{arr}", fx.org_id);
    let (_, html) = load_page(&ctx.app, &browser, &page).await;
    post_form(
        &ctx.app,
        &browser,
        &format!("{page}/annotations"),
        &[("type", "general"), ("content", "x")],
        &form_csrf(&html),
    )
    .await;
    let (_, html) = load_page(&ctx.app, &browser, &page).await;
    assert!(html.contains("<time datetime=\""), "{html}");
    assert!(html.contains(" UTC</time>"));
}
