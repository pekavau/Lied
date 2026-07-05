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
use dav_server::DavHandler;

use crate::auth::webdav::{authenticate, AuthenticatedWebDavUser};
use crate::state::AppState;
use crate::webdav::LiedFs;

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/orgs", any(dav))
        .route("/orgs/*path", any(dav))
        .route("/users", any(dav))
        .route("/users/*path", any(dav))
        // `route_layer`, not `layer`: the app-password Basic challenge must
        // apply ONLY to these matched WebDAV routes, never to the router's
        // fallback. A plain `.layer()` also wraps the default fallback, and
        // `.merge()`ing this router into the app tree then makes that
        // auth-wrapped fallback the app-wide catch-all — so every unmatched
        // path (favicon, `/admin/`, typos) would answer `401 WWW-Authenticate:
        // Basic` instead of a clean 404 (issue #15, bug 1).
        .route_layer(middleware::from_fn_with_state(
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

/// The real WebDAV handler, reached only after successful app-password auth
/// (the [`AuthenticatedWebDavUser`] is in the request extensions). Builds a
/// per-request [`LiedFs`] scoped to that identity and drives the request
/// through `dav-server`, honoring soft-delete and per-role visibility.
async fn dav(State(state): State<AppState>, request: Request) -> Response {
    let Some(user) = request
        .extensions()
        .get::<AuthenticatedWebDavUser>()
        .cloned()
    else {
        // The auth middleware runs as a `route_layer` in front of this handler,
        // so a missing extension is an internal wiring error, not a client one.
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let request_id = request
        .headers()
        .get(crate::routes::REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| uuid::Uuid::parse_str(s).ok());

    // The WebDAV `/users/<slug>/library` tree is keyed by user slug; resolve it
    // once for the private-library ownership check.
    let user_slug =
        match sqlx::query_scalar!(r#"SELECT slug FROM "user" WHERE id = $1"#, user.user_id)
            .fetch_optional(&state.db)
            .await
        {
            Ok(Some(slug)) => slug,
            Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };

    let fs = LiedFs::new(state.clone(), user.user_id, user_slug, request_id);
    let handler = DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(dav_server::fakels::FakeLs::new())
        .build_handler();

    let response = handler.handle(request).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, axum::body::Body::new(body))
}
