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
pub mod arrangements;
pub mod files;
pub mod infra;
pub mod orgs;
pub mod v1;
pub mod webdav;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderName, Request};
use axum::Router;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::state::AppState;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The current request's id, parsed from the `x-request-id` header that
/// [`SetRequestIdLayer`] stamps on every inbound request (and that the trace
/// span logs). Handlers take this extractor and pass it into [`audit`] so the
/// audit row's `request_id` correlates with the tracing span — that is the
/// whole reason the column exists (CLAUDE.md Security baseline: audit
/// `request_id` "correlates with the tracing span").
///
/// Falls back to a fresh id only if the header is somehow absent or
/// unparseable (it shouldn't be: the layer runs outermost, before routing),
/// so an audit row never carries a meaningless `NULL` for a real HTTP request.
///
/// [`audit`]: crate::domain::audit_log::audit
#[derive(Debug, Clone, Copy)]
pub struct RequestId(pub Uuid);

#[async_trait::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for RequestId {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(RequestId(request_id_from_parts(parts)))
    }
}

/// Read and parse the `x-request-id` header from request parts, falling back
/// to a fresh id if absent/unparseable. Shared by the [`RequestId`] extractor
/// and middleware (which has `Parts`/`Request` but can't run an extractor).
pub fn request_id_from_parts(parts: &Parts) -> Uuid {
    parts
        .headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::now_v7)
}

/// Compose the full application router: the four sibling trees plus shared
/// middleware (request-ID generation/propagation, request tracing).
pub fn build_router(state: AppState) -> Router {
    let header_name = HeaderName::from_static(REQUEST_ID_HEADER);

    Router::new()
        .merge(infra::router())
        .nest("/admin", admin::router(&state))
        .nest("/v1", v1::router(state.clone()))
        .merge(webdav::router(&state))
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
