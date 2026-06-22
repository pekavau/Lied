//! `/v1/...` — JSON REST tree. Session cookie or bearer token auth.
//! Empty stub for this scaffold item; domain entities land in later
//! phase-1 items.

use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionInfo {
    api_version: &'static str,
}

pub fn router(_state: AppState) -> Router<AppState> {
    Router::new().route("/", get(version))
}

async fn version() -> Json<VersionInfo> {
    Json(VersionInfo { api_version: "v1" })
}
