//! WebDAV tree: `/orgs/<org-slug>/...` and `/users/<user-slug>/library/...`.
//! App-password (HTTP Basic) auth.
//!
//! Real `dav-server` wiring (org/user-scoped filesystem backends honoring
//! soft-delete and per-role visibility) lands once Organization/User/Voice/
//! File CRUD exists. This item (issue #4) wires the *authentication*
//! middleware in front of the tree: a request with a valid app password
//! reaches the (still-stub) handler; without one, it gets `401` +
//! `WWW-Authenticate: Basic` before the handler ever runs.

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

use crate::auth::webdav::authenticate;
use crate::state::AppState;

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/orgs", any(stub))
        .route("/orgs/*path", any(stub))
        .route("/users", any(stub))
        .route("/users/*path", any(stub))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            app_password_auth,
        ))
}

/// Authenticates every request under `/orgs` and `/users` via HTTP Basic +
/// app password (CLAUDE.md AppPassword entity). On success, the
/// authenticated user id/username are inserted into the request
/// extensions for downstream handlers; on failure, short-circuits with
/// `401` + `WWW-Authenticate: Basic` (RFC 7617) without reaching any
/// handler.
async fn app_password_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let header_value = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Correlate the audit row with the tracing span via the real request id
    // stamped by `SetRequestIdLayer`. This is middleware, so we read the
    // header off the `Request` directly rather than running the `RequestId`
    // extractor.
    let request_id = request
        .headers()
        .get(crate::routes::REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| uuid::Uuid::parse_str(s).ok());

    match authenticate(&state.db, header_value.as_deref(), request_id).await {
        Ok(user) => {
            request.extensions_mut().insert(user);
            next.run(request).await
        }
        Err(_) => unauthorized_response(),
    }
}

fn unauthorized_response() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Lied WebDAV\""),
    );
    response
}

/// Stub handler reached only after successful app-password auth. Returns
/// `207 Multi-Status` (the expected PROPFIND response code) so the
/// auth-only acceptance criterion ("PROPFIND with a valid app password →
/// 207 / non-401") is satisfiable today; actual WebDAV semantics (real
/// PROPFIND XML bodies, GET/PUT file content, directory listings honoring
/// soft-delete and per-role visibility) are later items.
async fn stub() -> impl IntoResponse {
    StatusCode::from_u16(207).unwrap_or(StatusCode::OK)
}
