//! The four sibling route trees, composed onto one axum `Router`.
//!
//! - `/admin/...`   — HTML fragments (HTMX; session cookie + CSRF). Stub in
//!   this skeleton.
//! - `/v1/...`      — JSON REST (session cookie or bearer token). Stub in
//!   this skeleton.
//! - `/orgs/...`, `/users/.../library/...` — WebDAV (app-password auth).
//!   Stub in this skeleton.
//! - `/healthz`, `/readyz`, `/metrics`, `/openapi.json`, `/docs` — infra,
//!   unauthenticated.
//!
//! Each tree gets the shared middleware stack (request-ID, tracing) applied
//! uniformly; auth/rate-limit layers per tree are deferred to later items
//! since there are no entities/sessions yet to authenticate against.

pub mod admin;
pub mod infra;
pub mod v1;
pub mod webdav;

use axum::http::{HeaderName, Request};
use axum::Router;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::state::AppState;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Compose the full application router: the four sibling trees plus shared
/// middleware (request-ID generation/propagation, request tracing).
pub fn build_router(state: AppState) -> Router {
    let header_name = HeaderName::from_static(REQUEST_ID_HEADER);

    Router::new()
        .merge(infra::router())
        .nest("/admin", admin::router())
        .nest("/v1", v1::router(state.clone()))
        .merge(webdav::router())
        .with_state(state)
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                let request_id = request
                    .headers()
                    .get(REQUEST_ID_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("unknown")
                    .to_string();
                tracing::info_span!(
                    "request",
                    request_id = %request_id,
                    method = %request.method(),
                    uri = %request.uri(),
                )
            }),
        )
        .layer(PropagateRequestIdLayer::new(header_name.clone()))
        .layer(SetRequestIdLayer::new(header_name, MakeRequestUuid))
}

/// Generate a fresh request ID. Exposed for handlers/tests that need to
/// stamp an ID outside the middleware path (e.g. constructing an `AppError`
/// `instance` field before the response layer sees it).
pub fn new_request_id() -> Uuid {
    Uuid::now_v7()
}
