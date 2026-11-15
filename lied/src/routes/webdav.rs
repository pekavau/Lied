//! WebDAV tree: `/orgs/<org-slug>/...` and `/users/<user-slug>/library/...`.
//! App-password (HTTP Basic) auth.
//!
//! [`app_password_auth`] authenticates every request (401 +
//! `WWW-Authenticate: Basic` on failure, before any handler runs); the [`dav`]
//! handler then builds a per-request [`LiedFs`] scoped to that identity and
//! drives the request through `dav-server`, honoring soft-delete and per-role
//! visibility (issue #8). The filesystem backend lives in [`crate::webdav`].

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
/// app password (docs/design.md AppPassword entity). On success, the
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
    let user_slug = match crate::domain::user::find_by_id(&state.db, user.user_id).await {
        Ok(Some(found)) => found.slug,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    if let Some(refusal) = refuse_before_dav(&request) {
        return refusal;
    }

    let fs = LiedFs::new(state.clone(), user.user_id, user_slug, request_id);
    let handler = DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(dav_server::fakels::FakeLs::new())
        .build_handler();

    let response = handler.handle(request).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, axum::body::Body::new(body))
}

/// Requests refused with 403 before dav-server runs, because dav-server
/// would delete things first and ask the filesystem second (#26):
/// - MOVE and COPY outside the private library (and a personal annotation's
///   own directory): an existing destination is deleted before the rename,
///   and in the org tree that is a real file
///   ([`crate::webdav::fs::copy_move_permitted`]);
/// - DELETE of an org-tree directory: its files are deleted before the
///   directory removal is refused ([`crate::webdav::fs::delete_permitted`]).
fn refuse_before_dav(request: &Request) -> Option<Response> {
    let decoded = |uri: &axum::http::Uri| -> Option<String> {
        let path = dav_server::davpath::DavPath::from_uri(uri).ok()?;
        String::from_utf8(path.as_bytes().to_vec()).ok()
    };
    let is_move = match request.method().as_str() {
        "MOVE" => true,
        "COPY" => false,
        "DELETE" => {
            // An unparsable path is dav-server's (and the filesystem's) 404.
            let path = decoded(request.uri())?;
            return (!crate::webdav::fs::delete_permitted(&path))
                .then(|| StatusCode::FORBIDDEN.into_response());
        }
        _ => return None,
    };
    // A missing or unparsable Destination is dav-server's to answer (400).
    let destination = request
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<axum::http::Uri>().ok())?;
    let (from, to) = (decoded(request.uri())?, decoded(&destination)?);
    (!crate::webdav::fs::copy_move_permitted(is_move, &from, &to))
        .then(|| StatusCode::FORBIDDEN.into_response())
}
