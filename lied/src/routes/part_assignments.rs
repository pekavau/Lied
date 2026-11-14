//! `/v1/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments` —
//! issue #10 (Part assignments + the archivist→musician loop).
//!
//! Adds a user as a player of a voice on a `CollectionItem`; a part takes as
//! many players as the section has (CLAUDE.md Unique-constraints:
//! `(collection_item_id, voice_id, user_id)`, issue #53). The assignment is
//! what grants a guest/substitute (no `Membership`) read access to that voice's
//! files via the WebDAV collections subtree — see `webdav::access` /
//! `webdav::fs`.
//!
//! Reads require any org membership (`musician`+); writes (add/remove a player,
//! set `notifiedAt`/`acknowledgedAt`, unassign) require
//! `owner`/`archivist`/`conductor` (CLAUDE.md Permission matrix: "Part
//! assignments"), enforced via [`require_collection_editor_v1`]. Optimistic
//! concurrency (ETag/`If-Match`), RFC 7807 errors, and an `audit()` on every
//! write, per the API guidelines. `part_assignment` has no soft-delete —
//! unassign is a hard delete.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::{require_collection_editor_v1, require_org_role_v1};
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::Role;
use crate::domain::{collection_item, part_assignment};
use crate::error::AppError;
use crate::listing::{self, etag_response, etag_response_status, if_match_header};
use crate::pagination::Page;
use crate::routes::json::ApiJson;
use crate::routes::openapi::{
    CommonErrors, Conflict409, Forbidden403, JsonBodyErrors, NotFound404, Precondition412,
    Validation400,
};
use crate::routes::patch::present_or_null;
use crate::routes::RequestId;
// Problem Details on a bad parameter (issue #66); see `routes::params`.
use crate::routes::params::{ApiPath as Path, ApiQuery as Query};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_assignments, assign_voice))
        .routes(routes!(
            get_assignment,
            update_assignment,
            delete_assignment
        ))
}

fn part_assignment_error_to_app_error(err: part_assignment::PartAssignmentError) -> AppError {
    match err {
        part_assignment::PartAssignmentError::Duplicate => {
            AppError::Conflict("this musician already plays this voice on this piece".to_string())
        }
        part_assignment::PartAssignmentError::VoiceNotInArrangement => AppError::field(
            "voiceId",
            "voice does not exist or does not belong to this item's arrangement",
        ),
        part_assignment::PartAssignmentError::UnknownReference => {
            AppError::field("userId", "user does not exist")
        }
        part_assignment::PartAssignmentError::Database(e) => AppError::Database(e),
    }
}

/// Resolve an item, verifying the collection belongs to the org and the item
/// belongs to the collection.
async fn find_item_scoped(
    state: &AppState,
    org_id: Uuid,
    collection_id: Uuid,
    item_id: Uuid,
) -> Result<collection_item::CollectionItem, AppError> {
    crate::routes::collections::find_collection_scoped(state, org_id, collection_id).await?;
    let found = collection_item::find_by_id(&state.db, item_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.collection_id != collection_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

/// Resolve an assignment, verifying the whole `org -> collection -> item ->
/// assignment` chain. `part_assignment` carries no `organization_id` — the
/// only path to the org is through its `collection_item` (CLAUDE.md: "Every
/// scoped finder MUST verify the collection_item belongs to the org in the
/// path, else an archivist of org B could assign into org A").
async fn find_assignment_scoped(
    state: &AppState,
    org_id: Uuid,
    collection_id: Uuid,
    item_id: Uuid,
    assignment_id: Uuid,
) -> Result<part_assignment::PartAssignment, AppError> {
    find_item_scoped(state, org_id, collection_id, item_id).await?;
    let found = part_assignment::find_by_id(&state.db, assignment_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.collection_item_id != item_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

// ── list ─────────────────────────────────────────────────────────────────────

/// List a collection item's part assignments (requires org membership).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments",
    tag = "part-assignments",
    summary = "List part assignments for a collection item",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
        ("itemId"       = Uuid, Path, description = "Collection item ID"),
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated assignment list",
            body = inline(Page<part_assignment::PartAssignment>)),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn list_assignments(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    Query(params): Query<crate::pagination::PageParams>,
) -> Result<Json<Page<part_assignment::PartAssignment>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    find_item_scoped(&state, org_id, collection_id, item_id).await?;

    let (limit, offset) =
        params.resolve(state.config.default_page_size, state.config.max_page_size);

    let (items, total) =
        part_assignment::list_for_item(&state.db, item_id, i64::from(limit), i64::from(offset))
            .await?;

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

// ── assign / reassign ───────────────────────────────────────────────────────

/// Request body for `POST .../assignments`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssignRequest {
    user_id: Uuid,
    voice_id: Uuid,
}

/// Add a musician to a voice on a collection item (owner/archivist/conductor).
///
/// **Adds a player; it does not replace one.** A part is played by as many
/// musicians as the section has (issue #53), so this is a `POST` that appends
/// to the assignments collection: always `201`, and `409` when the same person
/// is added to the same voice twice.
///
/// There is no `If-Match` here because nothing is being overwritten. Removing a
/// player is `DELETE` by assignment id and changing distribution state is
/// `PATCH` by assignment id; both keep their optimistic-concurrency contract.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments",
    tag = "part-assignments",
    summary = "Add a musician to a voice on a collection item",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
        ("itemId"       = Uuid, Path, description = "Collection item ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = AssignRequest,
    responses(
        (status = 201, description = "Musician added to the voice", body = part_assignment::PartAssignment,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        JsonBodyErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Conflict409,
    )
)]
async fn assign_voice(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    ApiJson(body): ApiJson<AssignRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_item_scoped(&state, org_id, collection_id, item_id).await?;

    // Nothing is overwritten, so there is no precondition to check. An unknown
    // `userId` is surfaced by the domain's FK-violation mapping (→ 400 on
    // `userId`), so no separate existence pre-check is needed here.
    let id = Uuid::now_v7();
    let created = part_assignment::assign(
        &state.db,
        id,
        item_id,
        body.voice_id,
        body.user_id,
        Some(auth.user.id),
    )
    .await
    .map_err(part_assignment_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "part_assignment.create",
        "part_assignment",
        Some(created.id),
        serde_json::json!({
            "collectionItemId": item_id,
            "voiceId": created.voice_id,
            "userId": created.user_id,
        }),
    )
    .await;

    let updated_at = created.updated_at;
    Ok(etag_response_status(
        StatusCode::CREATED,
        created,
        updated_at,
    ))
}

// ── get / update / delete a single assignment ───────────────────────────────

/// Fetch one part assignment (requires org membership).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments/{assignmentId}",
    tag = "part-assignments",
    summary = "Get a part assignment",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
        ("itemId"       = Uuid, Path, description = "Collection item ID"),
        ("assignmentId" = Uuid, Path, description = "Assignment ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Assignment", body = part_assignment::PartAssignment,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_assignment(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id, item_id, assignment_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    let found =
        find_assignment_scoped(&state, org_id, collection_id, item_id, assignment_id).await?;
    let updated_at = found.updated_at;
    Ok(etag_response(found, updated_at))
}

/// Request body for `PATCH .../assignments/{assignmentId}` — sets or clears
/// `notifiedAt`/`acknowledgedAt`. The notification/acknowledgement
/// *workflow* is deferred (CLAUDE.md phase-1 cut), but the fields are
/// settable now. A partial update: an omitted field is kept, `null` clears it.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateAssignmentStateRequest {
    #[serde(default, deserialize_with = "present_or_null")]
    #[schema(value_type = Option<DateTime<Utc>>, nullable)]
    notified_at: Option<Option<DateTime<Utc>>>,
    #[serde(default, deserialize_with = "present_or_null")]
    #[schema(value_type = Option<DateTime<Utc>>, nullable)]
    acknowledged_at: Option<Option<DateTime<Utc>>>,
}

/// Set `notifiedAt`/`acknowledgedAt` on an assignment (owner/archivist/
/// conductor; requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments/{assignmentId}",
    tag = "part-assignments",
    summary = "Update a part assignment's notified/acknowledged state",
    params(
        ("orgId"        = Uuid,   Path,   description = "Organization ID"),
        ("collectionId" = Uuid,   Path,   description = "Collection ID"),
        ("itemId"       = Uuid,   Path,   description = "Collection item ID"),
        ("assignmentId" = Uuid,   Path,   description = "Assignment ID"),
        ("If-Match"     = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateAssignmentStateRequest,
    responses(
        (status = 200, description = "Updated assignment", body = part_assignment::PartAssignment,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        JsonBodyErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Conflict409,
        Precondition412,
    )
)]
async fn update_assignment(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, item_id, assignment_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<UpdateAssignmentStateRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current =
        find_assignment_scoped(&state, org_id, collection_id, item_id, assignment_id).await?;
    crate::listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let notified_at = part_assignment::StampChange::from(body.notified_at);
    let acknowledged_at = part_assignment::StampChange::from(body.acknowledged_at);
    // Compared at the stored (millisecond) precision, so re-sending the value
    // a GET returned is recognised as no change.
    let same = |a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>| {
        a.map(|d| d.timestamp_millis()) == b.map(|d| d.timestamp_millis())
    };
    if same(notified_at.apply(current.notified_at), current.notified_at)
        && same(
            acknowledged_at.apply(current.acknowledged_at),
            current.acknowledged_at,
        )
    {
        let updated_at = current.updated_at;
        return Ok(etag_response(current, updated_at));
    }

    let Some(updated) = part_assignment::update_state(
        &state.db,
        assignment_id,
        Some(listing::version_of(current.updated_at)),
        notified_at,
        acknowledged_at,
    )
    .await?
    else {
        return Err(listing::stale_or_gone(find_assignment_scoped(
            &state,
            org_id,
            collection_id,
            item_id,
            assignment_id,
        ))
        .await);
    };

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "part_assignment.update_state",
        "part_assignment",
        Some(assignment_id),
        serde_json::json!({
            "before": { "notifiedAt": current.notified_at, "acknowledgedAt": current.acknowledged_at },
            "after": { "notifiedAt": updated.notified_at, "acknowledgedAt": updated.acknowledged_at },
        }),
    )
    .await;

    let updated_at = updated.updated_at;
    Ok(etag_response(updated, updated_at))
}

/// Unassign — hard-delete the assignment (owner/archivist/conductor;
/// requires `If-Match`). No soft-delete on this entity.
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments/{assignmentId}",
    tag = "part-assignments",
    summary = "Unassign a part",
    params(
        ("orgId"        = Uuid,   Path,   description = "Organization ID"),
        ("collectionId" = Uuid,   Path,   description = "Collection ID"),
        ("itemId"       = Uuid,   Path,   description = "Collection item ID"),
        ("assignmentId" = Uuid,   Path,   description = "Assignment ID"),
        ("If-Match"     = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Assignment removed"),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_assignment(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, item_id, assignment_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current =
        find_assignment_scoped(&state, org_id, collection_id, item_id, assignment_id).await?;
    crate::listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if !part_assignment::delete(
        &state.db,
        assignment_id,
        Some(listing::version_of(current.updated_at)),
    )
    .await?
    {
        return Err(listing::stale_or_gone(find_assignment_scoped(
            &state,
            org_id,
            collection_id,
            item_id,
            assignment_id,
        ))
        .await);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "part_assignment.delete",
        "part_assignment",
        Some(assignment_id),
        serde_json::json!({
            "collectionItemId": item_id,
            "voiceId": current.voice_id,
            "userId": current.user_id,
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}
