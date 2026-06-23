//! `/v1/...` — JSON REST tree. Session cookie or bearer token auth.
//!
//! `GET /v1/instruments` is the first real domain endpoint (phase-1 item 2);
//! it is temporarily open (no auth) — see the `TODO(#4)` below. Remaining
//! domain entities land in later phase-1 items.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Serialize;

use crate::domain::instrument;
use crate::error::AppError;
use crate::pagination::{Page, PageParams};
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionInfo {
    api_version: &'static str,
}

pub fn router(_state: AppState) -> Router<AppState> {
    Router::new()
        .route("/", get(version))
        .route("/instruments", get(list_instruments))
}

async fn version() -> Json<VersionInfo> {
    Json(VersionInfo { api_version: "v1" })
}

/// `GET /v1/instruments?limit=&offset=` — paginated list of the
/// instance-wide instrument vocabulary, ordered by display name.
///
/// TODO(#4): auth-gate. Phase-1 item 2 leaves this endpoint open (no
/// session/bearer check) since auth lands in issue #4; reads are
/// low-sensitivity (a static controlled vocabulary), so this is a
/// deliberate, temporary exception to the rest of the `/v1` tree.
async fn list_instruments(
    State(state): State<AppState>,
    Query(params): Query<PageParams>,
) -> Result<Json<Page<instrument::Instrument>>, AppError> {
    let (limit, offset) =
        params.resolve(state.config.default_page_size, state.config.max_page_size);

    let (items, total) = instrument::list(&state.db, i64::from(limit), i64::from(offset)).await?;

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}
