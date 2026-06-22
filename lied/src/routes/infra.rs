//! Infra tree: `/healthz`, `/readyz`, `/metrics`, `/openapi.json`, `/docs`.
//!
//! Unauthenticated by design (see CLAUDE.md "Route-tree boundary").

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use utoipa::OpenApi;
use utoipa_rapidoc::RapiDoc;

use crate::state::AppState;

/// Upper bound on each `/readyz` dependency check. A readiness probe must
/// answer quickly so a load balancer can shed traffic; without this a dead
/// dependency would stall the probe for the pool/SDK timeout (tens of
/// seconds) instead of returning a prompt 503.
const READYZ_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(OpenApi)]
#[openapi(info(title = "Lied API", version = "0.1.0"))]
struct ApiDoc;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/openapi.json", get(openapi_json))
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
async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    if !state.config.metrics_enabled {
        return (StatusCode::NOT_FOUND, String::new());
    }

    // A full Prometheus exporter handle would normally be installed once at
    // startup and rendered here; the skeleton renders an empty registry so
    // the endpoint shape (and the gate) is correct ahead of real metrics
    // being recorded in later items.
    let handle = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    (StatusCode::OK, handle.handle().render())
}

async fn openapi_json() -> impl IntoResponse {
    axum::Json(ApiDoc::openapi())
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
