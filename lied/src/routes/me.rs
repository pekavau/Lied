//! `GET /v1/me`: who the caller is and where they belong (issue #97).
//!
//! A client that has just logged in (cookie or bearer token) knows only what
//! it sent; this is how it learns the user's id, name, system-admin flag and
//! the organizations they can enter. `/v1/me/...` is the home of the
//! caller's own views (the "my parts" list of #98 belongs here too).
//!
//! No `ETag`: the response combines the user row with every membership, so
//! it has no single `updated_at` to version, and nothing is written through
//! this path (an `If-Match` would have nothing to guard).

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::extractors::BearerOrSession;
use crate::domain::membership::{self, Role};
use crate::domain::user::User;
use crate::error::AppError;
use crate::routes::openapi::CommonErrors;
use crate::state::AppState;

/// The organization a membership belongs to, as much as a client needs to
/// show and enter it.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeOrganization {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
}

/// One of the caller's memberships, with its organization.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeMembership {
    /// The membership's id, as under `/v1/orgs/{orgId}/members/{memberId}`.
    pub id: Uuid,
    pub organization: MeOrganization,
    pub role: Role,
    pub instrument_ids: Vec<Uuid>,
    pub is_principal: bool,
    pub principal_instrument_ids: Vec<Uuid>,
}

/// The caller and their memberships.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeResponse {
    pub user: User,
    /// Every organization the caller is a member of, ordered by organization
    /// name; empty for a guest or a system admin without memberships. Not
    /// paginated: one entry per organization the caller belongs to.
    pub memberships: Vec<MeMembership>,
}

/// The authenticated caller and their memberships.
///
/// A system admin reaches every organization, but only those with a
/// membership are listed here; `isSystemAdmin` tells the client the rest.
#[utoipa::path(
    get,
    path = "/me",
    tag = "me",
    summary = "Who am I",
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "The caller and their memberships", body = MeResponse),
        CommonErrors,
    )
)]
async fn get_me(
    auth: BearerOrSession,
    State(state): State<AppState>,
) -> Result<Json<MeResponse>, AppError> {
    let memberships = membership::list_for_user(&state.db, auth.user.id)
        .await?
        .into_iter()
        .map(|m| MeMembership {
            id: m.membership_id,
            organization: MeOrganization {
                id: m.organization_id,
                name: m.organization_name,
                slug: m.organization_slug,
            },
            role: m.role,
            instrument_ids: m.instrument_ids,
            is_principal: m.is_principal,
            principal_instrument_ids: m.principal_instrument_ids,
        })
        .collect();
    Ok(Json(MeResponse {
        user: auth.user,
        memberships,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_me))
}
