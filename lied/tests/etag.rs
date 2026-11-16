//! The `ETag` the OpenAPI contract declares is the `ETag` the server sends
//! (#125).
//!
//! The spec declares an `ETag` response header on every create (201), read
//! (200) and update (200) of a versioned resource, but several creates used to
//! answer with a bare `(201, Json)`, so a client built from the spec looked for
//! a header that never came. This test walks the spec: every operation that
//! declares an `ETag` must be driven below, and its response must carry one.
//! An operation that gains an `ETag` in the spec without being added here fails
//! the coverage check, so the two cannot drift apart.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::http::{header, Request, StatusCode};
use lied::config::{AppConfig, Secret};
use lied::domain::{membership, organization, user};
use lied::routes::openapi::ApiDoc;
use lied::state::AppState;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use testcontainers::runners::AsyncRunner;
use testcontainers::ImageExt;
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use uuid::Uuid;

fn test_state(pool: sqlx::PgPool) -> AppState {
    let config = AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from("postgres://unused/unused".to_string()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        s3_version_retention_days: 30,
        jwt_signing_key: Secret::from("test-signing-key-material-for-etag-tests".to_string()),
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
    AppState {
        config: Arc::new(config),
        db: pool,
        s3: aws_sdk_s3::Client::from_conf(s3_config),
        metrics: None,
        jwt_keyring,
    }
}

/// Every `(METHOD, path template)` the spec declares an `ETag` response
/// header on, from the document `build_router` serves.
fn declared_etag_operations() -> BTreeSet<(String, String)> {
    let (_, api) = OpenApiRouter::<AppState>::with_openapi(ApiDoc::openapi())
        .nest("/v1", lied::routes::v1::api_router(10))
        .split_for_parts();
    let doc = serde_json::to_value(api).expect("spec serializes");
    let mut out = BTreeSet::new();
    for (path, item) in doc["paths"].as_object().expect("paths") {
        for (method, op) in item.as_object().expect("path item") {
            let Some(responses) = op.get("responses").and_then(Value::as_object) else {
                continue;
            };
            let declares = responses.values().any(|r| {
                r.get("headers")
                    .and_then(Value::as_object)
                    .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("etag")))
            });
            if declares {
                out.insert((method.to_uppercase(), path.clone()));
            }
        }
    }
    out
}

struct Client {
    app: axum::Router,
    token: String,
    /// Operations driven so far, as `(METHOD, path template)`.
    driven: BTreeSet<(String, String)>,
    /// Operations whose response lacked an `ETag`.
    missing: Vec<String>,
}

impl Client {
    /// Send `method uri` (`template` is its spec path), record whether the
    /// response carried an `ETag`, and return the status, the `ETag` and the
    /// JSON body.
    async fn call(
        &mut self,
        method: &str,
        template: &str,
        uri: &str,
        if_match: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Option<String>, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token));
        if let Some(tag) = if_match {
            builder = builder.header(header::IF_MATCH, tag);
        }
        let request = match body {
            Some(v) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(v.to_string()))
                .unwrap(),
            None => builder.body(axum::body::Body::empty()).unwrap(),
        };
        let response = self.app.clone().oneshot(request).await.expect("request");
        let status = response.status();
        let etag = response
            .headers()
            .get(header::ETAG)
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert!(status.is_success(), "{method} {uri}: {status} {json}");
        self.driven
            .insert((method.to_string(), format!("/v1{template}")));
        if etag.is_none() {
            self.missing
                .push(format!("{method} /v1{template} ({status})"));
        }
        (status, etag, json)
    }

    /// Create (`POST`), read (`GET`) and update (`PATCH`, guarded by the
    /// read's `ETag`) one resource; returns the created body.
    async fn crud(
        &mut self,
        collection: (&str, &str),
        item: (&str, String),
        create: Value,
        update: Value,
    ) -> Value {
        let (status, _, created) = self
            .call("POST", collection.0, collection.1, None, Some(create))
            .await;
        assert_eq!(status, StatusCode::CREATED, "POST {}", collection.1);
        let id = created["id"].as_str().expect("created id").to_string();
        let uri = item.1.replace("{id}", &id);
        let (_, etag, _) = self.call("GET", item.0, &uri, None, None).await;
        let etag = etag.unwrap_or_else(|| "\"0\"".to_string());
        self.call("PATCH", item.0, &uri, Some(&etag), Some(update))
            .await;
        created
    }
}

#[tokio::test]
async fn every_declared_etag_is_sent() {
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

    // A system admin: owner-equivalent in every org, and the one caller who
    // may create users and orgs.
    let admin = Uuid::now_v7();
    user::create(&pool, admin, "root", "root", None, None, "Root", true, None)
        .await
        .expect("admin");
    let org = Uuid::now_v7();
    organization::create(&pool, org, "Kapelle", "kapelle", None)
        .await
        .expect("org");
    membership::create(
        &pool,
        Uuid::now_v7(),
        admin,
        org,
        membership::MembershipFields {
            role: membership::Role::Owner,
            instrument_ids: &[],
            is_principal: false,
            principal_instrument_ids: &[],
        },
        None,
    )
    .await
    .expect("owner membership");

    let state = test_state(pool.clone());
    let token = state.jwt_keyring.mint(admin, None).expect("mint").0;
    let mut c = Client {
        app: lied::routes::build_router(state),
        token,
        driven: BTreeSet::new(),
        missing: Vec::new(),
    };

    let instrument = sqlx::query_scalar::<_, Uuid>("SELECT id FROM instrument ORDER BY id LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("an instrument is seeded");

    // Users and orgs (system admin).
    let player = c
        .call(
            "POST",
            "/users",
            "/v1/users",
            None,
            Some(json!({ "username": "anna", "displayName": "Anna", "password": "a long password 1" })),
        )
        .await
        .2;
    let player = player["id"].as_str().unwrap().to_string();
    c.call(
        "GET",
        "/users/{userId}",
        &format!("/v1/users/{player}"),
        None,
        None,
    )
    .await;
    c.crud(
        ("/orgs", "/v1/orgs"),
        ("/orgs/{orgId}", "/v1/orgs/{id}".to_string()),
        json!({ "name": "Zweite Kapelle" }),
        json!({ "name": "Andere Kapelle" }),
    )
    .await;

    // Members.
    let o = format!("/v1/orgs/{org}");
    c.crud(
        ("/orgs/{orgId}/members", &format!("{o}/members")),
        (
            "/orgs/{orgId}/members/{memberId}",
            format!("{o}/members/{{id}}"),
        ),
        json!({ "userId": player, "role": "musician" }),
        json!({ "role": "conductor" }),
    )
    .await;

    // Works, arrangements, voices, tags, annotations.
    c.crud(
        ("/works", "/v1/works"),
        ("/works/{workId}", "/v1/works/{id}".to_string()),
        json!({ "title": "Florentiner Marsch" }),
        json!({ "title": "Florentiner" }),
    )
    .await;
    let arrangement = c
        .crud(
            ("/orgs/{orgId}/arrangements", &format!("{o}/arrangements")),
            (
                "/orgs/{orgId}/arrangements/{arrangementId}",
                format!("{o}/arrangements/{{id}}"),
            ),
            json!({ "title": "Florentiner" }),
            json!({ "title": "Florentiner Marsch" }),
        )
        .await;
    let a = format!("{o}/arrangements/{}", arrangement["id"].as_str().unwrap());
    let voice = c
        .crud(
            (
                "/orgs/{orgId}/arrangements/{arrangementId}/voices",
                &format!("{a}/voices"),
            ),
            (
                "/orgs/{orgId}/arrangements/{arrangementId}/voices/{voiceId}",
                format!("{a}/voices/{{id}}"),
            ),
            json!({ "name": "1. Flöte", "instrumentIds": [instrument] }),
            json!({ "name": "Flöte 1" }),
        )
        .await;
    let voice = voice["id"].as_str().unwrap().to_string();
    c.crud(
        ("/orgs/{orgId}/tags", &format!("{o}/tags")),
        ("/orgs/{orgId}/tags/{tagId}", format!("{o}/tags/{{id}}")),
        json!({ "name": "Marsch" }),
        json!({ "name": "Märsche" }),
    )
    .await;
    c.crud(
        (
            "/orgs/{orgId}/arrangements/{arrangementId}/annotations",
            &format!("{a}/annotations"),
        ),
        (
            "/orgs/{orgId}/arrangements/{arrangementId}/annotations/{annotationId}",
            format!("{a}/annotations/{{id}}"),
        ),
        json!({ "type": "tempo", "content": "zügig" }),
        json!({ "content": "sehr zügig" }),
    )
    .await;

    // Collections, items, assignments, sections.
    let program = c
        .crud(
            ("/orgs/{orgId}/collections", &format!("{o}/collections")),
            (
                "/orgs/{orgId}/collections/{collectionId}",
                format!("{o}/collections/{{id}}"),
            ),
            json!({ "name": "Frühjahrskonzert", "type": "program" }),
            json!({ "name": "Frühlingskonzert" }),
        )
        .await;
    let p = format!("{o}/collections/{}", program["id"].as_str().unwrap());
    let item = c
        .crud(
            (
                "/orgs/{orgId}/collections/{collectionId}/items",
                &format!("{p}/items"),
            ),
            (
                "/orgs/{orgId}/collections/{collectionId}/items/{itemId}",
                format!("{p}/items/{{id}}"),
            ),
            json!({ "arrangementId": arrangement["id"] }),
            json!({ "position": 1 }),
        )
        .await;
    let i = format!("{p}/items/{}", item["id"].as_str().unwrap());
    c.crud(
        (
            "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments",
            &format!("{i}/assignments"),
        ),
        (
            "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments/{assignmentId}",
            format!("{i}/assignments/{{id}}"),
        ),
        json!({ "userId": player, "voiceId": voice }),
        json!({ "notifiedAt": "2026-11-16T12:00:00Z" }),
    )
    .await;
    let standing = c
        .call(
            "POST",
            "/orgs/{orgId}/collections",
            &format!("{o}/collections"),
            None,
            Some(json!({ "name": "Marschbuch", "type": "standing" })),
        )
        .await
        .2;
    let s = format!("{o}/collections/{}", standing["id"].as_str().unwrap());
    c.crud(
        (
            "/orgs/{orgId}/collections/{collectionId}/sections",
            &format!("{s}/sections"),
        ),
        (
            "/orgs/{orgId}/collections/{collectionId}/sections/{sectionId}",
            format!("{s}/sections/{{id}}"),
        ),
        json!({ "name": "Märsche", "startsAt": 1 }),
        json!({ "name": "Konzertmärsche" }),
    )
    .await;

    let declared = declared_etag_operations();
    let undriven: Vec<_> = declared.difference(&c.driven).collect();
    assert!(
        undriven.is_empty(),
        "operations declaring an ETag that this test does not drive (add them): {undriven:?}"
    );
    assert!(
        c.missing.is_empty(),
        "responses missing the ETag their spec declares: {:#?}",
        c.missing
    );
}
