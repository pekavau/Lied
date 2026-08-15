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
pub mod collections;
pub mod files;
pub mod infra;
pub mod openapi;
pub mod orgs;
pub mod part_assignments;
pub mod v1;
pub mod webdav;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderName, Request};
use axum::Router;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use uuid::Uuid;

use crate::state::AppState;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Query parameters whose values must never reach a log sink, keyed by the
/// exact parameter name. Currently just the CSRF token that `multipart/*`
/// admin submits carry in the URL (see [`crate::auth::csrf`]): logs are
/// long-lived and widely readable, and a session's CSRF token stays valid for
/// that session's lifetime, so persisting one would hand a log reader the
/// secret half of the double-submit pair.
const REDACTED_QUERY_PARAMS: &[&str] = &[crate::auth::csrf::CSRF_QUERY_PARAM];

/// The request URI as it is safe to log: path plus query, with the value of any
/// [`REDACTED_QUERY_PARAMS`] replaced by `[redacted]` (the key is kept so the
/// log still shows the parameter was present — same convention as the audit
/// log's payload redaction).
pub fn sanitized_uri(uri: &axum::http::Uri) -> String {
    let Some(query) = uri.query() else {
        return uri.to_string();
    };
    if !REDACTED_QUERY_PARAMS
        .iter()
        .any(|name| query.split('&').any(|pair| is_param(pair, name)))
    {
        return uri.to_string();
    }
    let redacted = query
        .split('&')
        .map(|pair| {
            match REDACTED_QUERY_PARAMS
                .iter()
                .find(|name| is_param(pair, name))
            {
                Some(name) => format!("{name}=[redacted]"),
                None => pair.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{redacted}", uri.path())
}

/// Whether a `key=value` (or bare `key`) query pair names `param`.
fn is_param(pair: &str, param: &str) -> bool {
    pair.split_once('=').map(|(k, _)| k).unwrap_or(pair) == param
}

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

    // Build the `/v1` tree via `OpenApiRouter` so every `#[utoipa::path]`
    // annotation is registered automatically; `split_for_parts()` yields
    // the standard axum router (for composition) and the fully-populated
    // `OpenApi` document (for serving at `/openapi.json`).
    let (v1_router, api) = OpenApiRouter::with_openapi(openapi::ApiDoc::openapi())
        .nest("/v1", v1::router(state.clone()))
        .split_for_parts();

    Router::new()
        .merge(infra::router(api))
        .nest("/admin", admin::router(&state))
        .merge(v1_router)
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
                    uri = %sanitized_uri(request.uri()),
                )
            }),
        )
        .layer(PropagateRequestIdLayer::new(header_name.clone()))
        .layer(SetRequestIdLayer::new(header_name, MakeRequestUuid))
}

#[cfg(test)]
mod tests {
    use super::sanitized_uri;

    fn sanitize(uri: &str) -> String {
        sanitized_uri(&uri.parse().unwrap())
    }

    #[test]
    fn sanitized_uri_redacts_the_csrf_token_but_keeps_the_rest() {
        // The console's multipart forms carry the session's CSRF token in the
        // URL; the request log must never persist its value.
        assert_eq!(
            sanitize("/admin/orgs/1/arrangements/2/files?csrf=SECRET-TOKEN"),
            "/admin/orgs/1/arrangements/2/files?csrf=[redacted]"
        );
        // The key survives (so the log still shows it was sent) and unrelated
        // params stay readable for debugging.
        assert_eq!(
            sanitize("/admin/files?limit=50&csrf=SECRET-TOKEN&sort=name"),
            "/admin/files?limit=50&csrf=[redacted]&sort=name"
        );
        // A valueless or repeated occurrence is still covered.
        assert_eq!(
            sanitize("/admin/files?csrf"),
            "/admin/files?csrf=[redacted]"
        );
        assert_eq!(
            sanitize("/admin/files?csrf=a&csrf=b"),
            "/admin/files?csrf=[redacted]&csrf=[redacted]"
        );
    }

    #[test]
    fn sanitized_uri_leaves_untainted_uris_untouched() {
        assert_eq!(sanitize("/admin/orgs"), "/admin/orgs");
        assert_eq!(
            sanitize("/v1/arrangements?q=mozart&limit=50"),
            "/v1/arrangements?q=mozart&limit=50"
        );
        // A param that merely *contains* the name is not the token.
        assert_eq!(
            sanitize("/v1/x?csrfish=keep&my_csrf=keep"),
            "/v1/x?csrfish=keep&my_csrf=keep"
        );
    }
}
