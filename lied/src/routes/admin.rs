//! `/admin/...` — HTMX admin UI tree. HTML fragments, session-cookie + CSRF
//! auth. Empty stub for this scaffold item; entity CRUD lands in later
//! phase-1 items.

use axum::response::Html;
use axum::routing::get;
use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(index))
}

async fn index() -> Html<&'static str> {
    Html("<h1>Lied admin</h1><p>Scaffold stub.</p>")
}
