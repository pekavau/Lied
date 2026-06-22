//! WebDAV tree: `/orgs/<org-slug>/...` and `/users/<user-slug>/library/...`.
//! App-password auth. Empty stub for this scaffold item — real `dav-server`
//! wiring (with org/user-scoped filesystem backends honoring soft-delete
//! and per-role visibility) lands once Organization/User/Voice/File exist.

use axum::routing::get;
use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs", get(stub))
        .route("/users", get(stub))
}

async fn stub() -> &'static str {
    "WebDAV scaffold stub — not yet implemented"
}
