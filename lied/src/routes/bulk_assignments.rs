//! `/v1/orgs/{orgId}/collections/{collectionId}/bulk-assignments` — one player
//! on the same part in every piece of a collection (issue #55).
//!
//! Two calls, mirroring [`crate::domain::bulk_assign`]: `GET …/preview` says
//! what would happen per piece; `POST` writes the `(item, voice)` pairs the
//! client accepted from that preview. The write is all-or-nothing and refuses
//! with `409` if the collection changed in between, so a client never assigns
//! something it was not shown.
//!
//! Both require `owner`/`archivist`/`conductor` (CLAUDE.md Permission matrix:
//! "Part assignments") — the preview included, since it reveals which parts a
//! named user holds across the collection. Neither is paginated: both are
//! bounded by the collection's size, like the coverage report — an exception to
//! the list-endpoint envelope recorded in CLAUDE.md.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::require_collection_editor_v1;
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::AuditContext;
use crate::domain::bulk_assign::{self, PartKey, PlanItem};
use crate::domain::{instrument, part_assignment, user};
use crate::error::AppError;
use crate::routes::collections::find_collection_scoped;
use crate::routes::json::ApiJson;
use crate::routes::openapi::{
    CommonErrors, Conflict409, Forbidden403, JsonBodyErrors, NotFound404, Validation400,
};
use crate::routes::RequestId;
// Problem Details on a bad parameter (issue #66); see `routes::params`.
use crate::routes::params::{ApiPath as Path, ApiQuery as Query};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(preview_bulk_assignment))
        .routes(routes!(apply_bulk_assignment))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
struct PreviewQuery {
    /// The player to assign.
    user_id: Uuid,
    /// The instrument of the part.
    instrument_id: Uuid,
    /// The part number (the `1` of "Trumpet in Bb 1"). Omit to match voices
    /// that carry no part number, such as Timpani or Harp.
    part_number: Option<i16>,
}

/// The per-piece plan.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct PreviewResponse {
    /// One entry per live piece of the collection, in running order.
    items: Vec<PlanItem>,
}

/// Preview assigning one player to the same part in every piece of a
/// collection (owner/archivist/conductor).
///
/// A voice matches when it has `instrumentId` and the same `partNumber` (both
/// absent counts as the same). Each piece reports one outcome: `assign`,
/// `alreadyHeld`, `noMatch` (with the piece's voices for that instrument as
/// `candidates`), `ambiguous` (more than one match, listed in `candidates`), or
/// `removed`. `alsoHolds` lists other parts of the same piece the player
/// already holds — a conflict that is reported, not refused.
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/bulk-assignments/preview",
    tag = "part-assignments",
    summary = "Preview assigning a player to one part across a collection",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
        PreviewQuery,
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "What would happen to each piece", body = PreviewResponse),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Validation400,
    )
)]
async fn preview_bulk_assignment(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<PreviewQuery>,
) -> Result<Json<PreviewResponse>, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, collection_id).await?;

    if q.part_number.is_some_and(|n| n < 1) {
        return Err(AppError::field(
            "partNumber",
            "partNumber must be at least 1",
        ));
    }
    // Unknown ids would otherwise yield a plausible plan ("no piece has this
    // part", "holds nothing") for a request that names nothing real.
    if !instrument::exists(&state.db, q.instrument_id).await? {
        return Err(AppError::field("instrumentId", "instrument does not exist"));
    }
    if user::find_by_id(&state.db, q.user_id).await?.is_none() {
        return Err(AppError::field("userId", "user does not exist"));
    }

    let items = bulk_assign::plan(
        &state.db,
        collection_id,
        PartKey {
            instrument_id: q.instrument_id,
            part_number: q.part_number,
        },
        q.user_id,
    )
    .await?;
    Ok(Json(PreviewResponse { items }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairRequest {
    collection_item_id: Uuid,
    voice_id: Uuid,
}

/// Request body for `POST …/bulk-assignments`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApplyRequest {
    user_id: Uuid,
    /// The part being assigned — the same `instrumentId` / `partNumber` the
    /// preview was asked for. Every pair must still be that part, or the
    /// request is refused with `409`: a voice renumbered or re-instrumented
    /// since the preview is no longer the part the client was shown.
    instrument_id: Uuid,
    /// Omitted or `null` for a part without a number.
    #[serde(default)]
    part_number: Option<i16>,
    /// The `(collectionItemId, voiceId)` pairs to assign — normally the
    /// `assign` entries of a preview. At least one.
    assignments: Vec<PairRequest>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct HeldPair {
    collection_item_id: Uuid,
    voice_id: Uuid,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct ApplyResponse {
    /// Assignments created, in submission order.
    created: Vec<part_assignment::PartAssignment>,
    /// Submitted pairs the player already held; left unchanged.
    already_held: Vec<HeldPair>,
}

/// Assign one player to a list of `(piece, voice)` pairs of a collection, all
/// or nothing (owner/archivist/conductor).
///
/// Each created row is an ordinary part assignment. Pairs the player already
/// holds are reported in `alreadyHeld`, not duplicated, so the same request can
/// be re-sent after pieces are added. If any pair no longer names a live piece
/// of this collection and a live voice of its arrangement that is still the
/// requested part (`instrumentId` + `partNumber`), nothing is written and the
/// response is `409` — preview again. Audit rows are written in the same
/// transaction as the assignments.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/bulk-assignments",
    tag = "part-assignments",
    summary = "Assign a player to many parts of a collection at once",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = ApplyRequest,
    responses(
        (status = 200, description = "Assignments made", body = ApplyResponse),
        CommonErrors,
        JsonBodyErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Conflict409,
    )
)]
async fn apply_bulk_assignment(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<ApplyRequest>,
) -> Result<Json<ApplyResponse>, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, collection_id).await?;

    let pairs: Vec<(Uuid, Uuid)> = body
        .assignments
        .iter()
        .map(|p| (p.collection_item_id, p.voice_id))
        .collect();
    if body.part_number.is_some_and(|n| n < 1) {
        return Err(AppError::field(
            "partNumber",
            "partNumber must be at least 1",
        ));
    }
    let audit_ctx = AuditContext {
        actor_user_id: Some(auth.user.id),
        org_id: Some(org_id),
        request_id: Some(request_id),
    };
    let applied = bulk_assign::apply(
        &state.db,
        collection_id,
        PartKey {
            instrument_id: body.instrument_id,
            part_number: body.part_number,
        },
        body.user_id,
        &pairs,
        &audit_ctx,
    )
    .await
    .map_err(|err| match err {
        bulk_assign::BulkAssignError::Empty => {
            AppError::field("assignments", "at least one assignment is required")
        }
        bulk_assign::BulkAssignError::UnknownUser => {
            AppError::field("userId", "user does not exist")
        }
        bulk_assign::BulkAssignError::Stale => AppError::Conflict(
            "the collection changed since the preview: a piece or voice is gone, or is \
             no longer this part — preview again"
                .to_string(),
        ),
        bulk_assign::BulkAssignError::Database(e) => AppError::Database(e),
    })?;

    Ok(Json(ApplyResponse {
        created: applied.created,
        already_held: applied
            .already_held
            .into_iter()
            .map(|(collection_item_id, voice_id)| HeldPair {
                collection_item_id,
                voice_id,
            })
            .collect(),
    }))
}
