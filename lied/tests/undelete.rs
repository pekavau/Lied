//! Undelete when a live row has since taken the name (#125).
//!
//! The unique indexes on arrangement and voice slugs, and on tag name + kind,
//! cover live rows only, so a deleted entity's slug or name can be reused. Its
//! undelete then violates the index. That used to surface as a raw database
//! error, a `500` on `/v1` and in the console; it is a `409 Conflict` now, as
//! collection undelete already was, and the deleted row stays deleted.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{header, Request, StatusCode};
use lied::auth;
use lied::config::{AppConfig, Secret};
use lied::domain::{arrangement, membership, organization, tag, user, voice};
use lied::state::AppState;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "undelete-test-password";

struct Ctx {
    _pg: ContainerAsync<Postgres>,
    pool: PgPool,
    app: axum::Router,
    state: AppState,
    org: Uuid,
    archivist: Uuid,
}

fn test_state(pool: PgPool) -> AppState {
    let config = AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        s3_version_retention_days: 30,
        jwt_signing_key: Secret::from("test-signing-key-material-undelete".to_string()),
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

impl Ctx {
    async fn new() -> Self {
        let pg = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("start postgres");
        let port = pg.get_host_port_ipv4(5432).await.expect("pg port");
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&format!(
                "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
            ))
            .await
            .expect("connect pg");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");

        let org = Uuid::now_v7();
        organization::create(&pool, org, "Kapelle", "kapelle", None)
            .await
            .expect("org");
        let archivist = Uuid::now_v7();
        let hash = auth::password::hash_password(PASSWORD).expect("hash");
        user::create(
            &pool,
            archivist,
            "arch",
            "arch",
            None,
            Some(&hash),
            "Archivist",
            false,
            None,
        )
        .await
        .expect("user");
        membership::create(
            &pool,
            Uuid::now_v7(),
            archivist,
            org,
            membership::MembershipFields {
                role: membership::Role::Archivist,
                instrument_ids: &[],
                is_principal: false,
                principal_instrument_ids: &[],
            },
            None,
        )
        .await
        .expect("membership");

        let state = test_state(pool.clone());
        let app = lied::routes::build_router(state.clone());
        Self {
            _pg: pg,
            pool,
            app,
            state,
            org,
            archivist,
        }
    }

    async fn v1_post(&self, uri: &str) -> (StatusCode, Value) {
        let token = self
            .state
            .jwt_keyring
            .mint(self.archivist, None)
            .expect("mint")
            .0;
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.expect("request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Log the archivist into the console and POST the restore form at `uri`.
    async fn console_post(&self, uri: &str) -> StatusCode {
        let peer = |mut r: Request<axum::body::Body>| {
            r.extensions_mut()
                .insert(axum::extract::ConnectInfo(SocketAddr::from((
                    [127, 0, 0, 1],
                    9000,
                ))));
            r
        };
        let login = peer(
            Request::builder()
                .method("POST")
                .uri("/admin/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(axum::body::Body::from(format!(
                    "username=arch&password={PASSWORD}"
                )))
                .unwrap(),
        );
        let response = self.app.clone().oneshot(login).await.expect("login");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "login");
        let session = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok()?.split(';').next())
            .find(|c| c.starts_with(auth::session::SESSION_COOKIE_NAME))
            .expect("session cookie")
            .to_string();
        // A safe request hands out the readable CSRF cookie.
        let page = peer(
            Request::builder()
                .uri(format!("/admin/orgs/{}/arrangements", self.org))
                .header(header::COOKIE, &session)
                .body(axum::body::Body::empty())
                .unwrap(),
        );
        let response = self.app.clone().oneshot(page).await.expect("page");
        let csrf = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok()?.split(';').next())
            .find_map(|c| c.strip_prefix("lied_csrf="))
            .expect("csrf cookie")
            .to_string();
        let restore = peer(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::COOKIE, &session)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(axum::body::Body::from(format!("csrf_token={csrf}")))
                .unwrap(),
        );
        self.app
            .clone()
            .oneshot(restore)
            .await
            .expect("restore")
            .status()
    }

    async fn arrangement(&self, title: &str) -> Uuid {
        let id = Uuid::now_v7();
        arrangement::create(
            &self.pool,
            id,
            self.org,
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

    async fn voice(&self, arrangement_id: Uuid, name: &str) -> Uuid {
        let instrument: Uuid = sqlx::query_scalar("SELECT id FROM instrument ORDER BY key LIMIT 1")
            .fetch_one(&self.pool)
            .await
            .expect("instrument");
        let id = Uuid::now_v7();
        voice::create(
            &self.pool,
            id,
            arrangement_id,
            name,
            &voice::slugify(name),
            &[instrument],
            voice::NewPartNumber::Set(None),
            None,
        )
        .await
        .expect("voice");
        id
    }

    async fn tag(&self, name: &str) -> Uuid {
        let id = Uuid::now_v7();
        tag::create(&self.pool, id, self.org, name, Some("genre"), None)
            .await
            .expect("tag");
        id
    }

    async fn is_deleted(&self, table: &str, id: Uuid) -> bool {
        // `table` is one of three literals below, never input.
        sqlx::query_scalar(&format!(
            "SELECT deleted_at IS NOT NULL FROM {table} WHERE id = $1"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .expect("row")
    }
}

/// A deleted arrangement, a deleted voice and a deleted tag, each of whose
/// slug or name a live row has since taken. Returns their ids.
async fn reused(ctx: &Ctx) -> (Uuid, Uuid, Uuid, Uuid) {
    let old_arrangement = ctx.arrangement("Florentiner").await;
    arrangement::soft_delete(&ctx.pool, old_arrangement, None)
        .await
        .expect("delete arrangement");
    let arrangement = ctx.arrangement("Florentiner").await;

    let old_voice = ctx.voice(arrangement, "Flöte 1").await;
    voice::soft_delete(&ctx.pool, old_voice, None)
        .await
        .expect("delete voice");
    ctx.voice(arrangement, "Flöte 1").await;

    let old_tag = ctx.tag("Marsch").await;
    tag::soft_delete(&ctx.pool, old_tag, None)
        .await
        .expect("delete tag");
    ctx.tag("Marsch").await;

    (old_arrangement, arrangement, old_voice, old_tag)
}

#[tokio::test]
async fn v1_undelete_of_a_reused_slug_or_name_conflicts() {
    let ctx = Ctx::new().await;
    let (old_arrangement, arrangement, old_voice, old_tag) = reused(&ctx).await;
    let o = format!("/v1/orgs/{}", ctx.org);

    for (what, uri, table, id) in [
        (
            "arrangement",
            format!("{o}/arrangements/{old_arrangement}/undelete"),
            "arrangement",
            old_arrangement,
        ),
        (
            "voice",
            format!("{o}/arrangements/{arrangement}/voices/{old_voice}/undelete"),
            "voice",
            old_voice,
        ),
        (
            "tag",
            format!("{o}/tags/{old_tag}/undelete"),
            "tag",
            old_tag,
        ),
    ] {
        let (status, body) = ctx.v1_post(&uri).await;
        assert_eq!(status, StatusCode::CONFLICT, "{what}: {body}");
        assert_eq!(body["type"], "https://lied/errors/conflict", "{what}");
        assert!(
            body["detail"].as_str().unwrap_or("").contains("restoring"),
            "{what}: the detail says what to do: {body}"
        );
        assert!(ctx.is_deleted(table, id).await, "{what} stays deleted");
    }
}

#[tokio::test]
async fn console_restore_of_a_reused_slug_or_name_conflicts() {
    let ctx = Ctx::new().await;
    let (old_arrangement, arrangement, old_voice, old_tag) = reused(&ctx).await;
    let o = format!("/admin/orgs/{}", ctx.org);

    for (what, uri, table, id) in [
        (
            "arrangement",
            format!("{o}/arrangements/{old_arrangement}/undelete"),
            "arrangement",
            old_arrangement,
        ),
        (
            "voice",
            format!("{o}/arrangements/{arrangement}/voices/{old_voice}/undelete"),
            "voice",
            old_voice,
        ),
        (
            "tag",
            format!("{o}/tags/{old_tag}/undelete"),
            "tag",
            old_tag,
        ),
    ] {
        assert_eq!(ctx.console_post(&uri).await, StatusCode::CONFLICT, "{what}");
        assert!(ctx.is_deleted(table, id).await, "{what} stays deleted");
    }
}

#[tokio::test]
async fn undelete_still_restores_when_the_slug_is_free() {
    let ctx = Ctx::new().await;
    let id = ctx.arrangement("Radetzky").await;
    arrangement::soft_delete(&ctx.pool, id, None)
        .await
        .expect("delete");
    let (status, body) = ctx
        .v1_post(&format!("/v1/orgs/{}/arrangements/{id}/undelete", ctx.org))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!ctx.is_deleted("arrangement", id).await);
}
