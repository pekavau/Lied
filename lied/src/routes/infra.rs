//! Infra tree: `/healthz`, `/readyz`, `/metrics`, `/openapi.json`, `/docs`.
//!
//! Unauthenticated by design (see CLAUDE.md "Route-tree boundary").
//!
//! `router(api)` takes the fully-populated [`utoipa::openapi::OpenApi`] built
//! by `routes/mod.rs` via `OpenApiRouter::split_for_parts()` and serves it at
//! `/openapi.json`. The stub `ApiDoc` that previously lived here has been
//! removed; the real document is now assembled from the per-handler
//! `#[utoipa::path]` annotations and the `routes!` macro across the whole `/v1`
//! tree (see `routes/openapi.rs` for the base `ApiDoc` seed).

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use utoipa_rapidoc::RapiDoc;

use crate::state::AppState;

/// Upper bound on each `/readyz` dependency check. A readiness probe must
/// answer quickly so a load balancer can shed traffic; without this a dead
/// dependency would stall the probe for the pool/SDK timeout (tens of
/// seconds) instead of returning a prompt 503.
const READYZ_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// Build the infra router. `api` is the fully-populated `OpenApi` document
/// produced by `OpenApiRouter::split_for_parts()` after all `/v1` handlers
/// have been registered; it replaces the empty stub that used to live here.
pub fn router(api: utoipa::openapi::OpenApi) -> Router<AppState> {
    // Serialize the spec once at startup, not on every request. `/openapi.json`
    // is unauthenticated and outside the rate limiter, so re-cloning and
    // re-serializing the whole `OpenApi` per hit would be needless work on a
    // publicly reachable endpoint. `Bytes` clones are a refcount bump (the
    // buffer is shared), so the per-request cost is just building the response.
    let openapi_bytes = Bytes::from(
        serde_json::to_vec(&api).expect("OpenAPI document serializes to JSON at startup"),
    );

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        // Serve the real, fully-populated spec (not the stub), pre-serialized.
        .route(
            "/openapi.json",
            get(move || {
                let body = openapi_bytes.clone();
                async move { ([(header::CONTENT_TYPE, "application/json")], body) }
            }),
        )
        .route("/docs", get(docs))
}

/// Liveness probe: 200 whenever the process is up. Drives container-restart
/// decisions, so it deliberately does not touch Postgres or MinIO.
async fn healthz() -> impl IntoResponse {
    StatusCode::OK
}

/// Readiness probe: checks Postgres (`SELECT 1`) and MinIO (`HeadBucket`).
/// 503 if either is unreachable. Drives traffic gating without triggering a
/// restart, so a transient DB blip sheds load instead of cycling the
/// container.
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    // Each check is bounded by READYZ_CHECK_TIMEOUT: a timeout elapsing is
    // treated as "not ready" (the dependency is unreachable or too slow).
    let db_ok = matches!(
        tokio::time::timeout(
            READYZ_CHECK_TIMEOUT,
            sqlx::query("SELECT 1").execute(&state.db),
        )
        .await,
        Ok(Ok(_))
    );

    let s3_ok = matches!(
        tokio::time::timeout(
            READYZ_CHECK_TIMEOUT,
            state
                .s3
                .head_bucket()
                .bucket(&state.config.s3_bucket)
                .send(),
        )
        .await,
        Ok(Ok(_))
    );

    if db_ok && s3_ok {
        (StatusCode::OK, "ready")
    } else {
        tracing::warn!(db_ok, s3_ok, "readiness check failed");
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// Prometheus metrics, gated behind `LIED_METRICS_ENABLED` (default off)
/// since the endpoint is unauthenticated.
///
/// Renders the globally-installed recorder's handle (set up in
/// [`crate::state::AppState::connect`]). When metrics are disabled no
/// recorder is installed, so `state.metrics` is `None` and the endpoint
/// returns 404.
async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    match &state.metrics {
        Some(handle) => (StatusCode::OK, handle.render()),
        None => (StatusCode::NOT_FOUND, String::new()),
    }
}

/// Gate for the RapiDoc UI: 404 when `LIED_DOCS_ENABLED=false`. The
/// `RapiDoc` router mounted below serves the actual asset content when
/// enabled; this handler only short-circuits the top-level `/docs` path
/// when the feature is disabled.
async fn docs(State(state): State<AppState>) -> impl IntoResponse {
    if state.config.docs_enabled {
        axum::response::Html(RapiDoc::new("/openapi.json").path("/docs").to_html()).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_returns_200() {
        let app: Router<()> = Router::new().route("/healthz", get(healthz));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
