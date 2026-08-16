//! `/v1/orgs/{orgId}/collections/{collectionId}/items/{itemId}/assignments` —
//! issue #10 (Part assignments + the archivist→musician loop).
//!
//! Assigns a user + voice to a `CollectionItem`; reassignment replaces the
//! row (CLAUDE.md Unique-constraints: `(collection_item_id, voice_id)`). The
//! assignment is what grants a guest/substitute (no `Membership`) read access
//! to that voice's files via the WebDAV collections subtree — see
//! `webdav::access` / `webdav::fs`.
//!
//! Reads require any org membership (`musician`+); writes (assign/reassign,
//! set `notifiedAt`/`acknowledgedAt`, unassign) require
//! `owner`/`archivist`/`conductor` (CLAUDE.md Permission matrix: "Part
//! assignments"), enforced via [`require_collection_editor_v1`]. Optimistic
//! concurrency (ETag/`If-Match`), RFC 7807 errors, and an `audit()` on every
//! write, per the API guidelines. `part_assignment` has no soft-delete
//! (CLAUDE.md: "reassignment replaces the row") — unassign is a hard delete.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::{require_collection_editor_v1, require_org_role_v1};
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::Role;
use crate::domain::{collection, collection_item, part_assignment};
use crate::error::AppError;
use crate::pagination::Page;
use crate::routes::openapi::{
    CommonErrors, Conflict409, Forbidden403, NotFound404, Precondition412, Validation400,
};
use crate::routes::RequestId;
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

// ── shared helpers (duplicated per-module per existing convention — see
// routes::collections for the same shapes) ─────────────────────────────────

fn if_match_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
}

fn etag_response<T: Serialize>(
    body: T,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> axum::response::Response {
    etag_response_status(StatusCode::OK, body, updated_at)
}

fn etag_response_status<T: Serialize>(
    status: StatusCode,
    body: T,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> axum::response::Response {
    let etag = crate::listing::etag_for(updated_at);
    let mut response = Json(body).into_response();
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::ETAG,
        axum::http::HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("\"0\"")),
    );
    response
}

fn empty_field(field: &str, msg: &str) -> AppError {
    let mut report = garde::Report::new();
    report.append(garde::Path::new(field), garde::Error::new(msg.to_string()));
    AppError::Validation(report)
}

fn part_assignment_error_to_app_error(err: part_assignment::PartAssignmentError) -> AppError {
    match err {
        part_assignment::PartAssignmentError::Duplicate => {
            AppError::Conflict("this musician already plays this voice on this piece".to_string())
        }
        part_assignment::PartAssignmentError::VoiceNotInArrangement => empty_field(
            "voiceId",
            "voice does not exist or does not belong to this item's arrangement",
        ),
        part_assignment::PartAssignmentError::UnknownReference => {
            empty_field("userId", "user does not exist")
        }
        part_assignment::PartAssignmentError::Database(e) => AppError::Database(e),
    }
}

/// Resolve a collection, verifying it belongs to `org_id`.
async fn find_collection_scoped(
    state: &AppState,
    org_id: Uuid,
    id: Uuid,
) -> Result<collection::Collection, AppError> {
    let found = collection::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.organization_id != org_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

/// Resolve an item, verifying the collection belongs to the org and the item
/// belongs to the collection.
async fn find_item_scoped(
    state: &AppState,
    org_id: Uuid,
    collection_id: Uuid,
    item_id: Uuid,
) -> Result<collection_item::CollectionItem, AppError> {
    find_collection_scoped(state, org_id, collection_id).await?;
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

/// Request body for `PUT .../assignments`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct AssignRequest {
    user_id: Uuid,
    voice_id: Uuid,
}

/// Assign a user + voice to a collection item, or replace the current
/// assignee if that `(item, voice)` pair is already assigned
/// (owner/archivist/conductor). `PUT` rather than `POST`: the resource
/// identity is the `(collectionItemId, voiceId)` pair, not a client-chosen id,
/// so re-issuing the same request with a different `userId` is a replace, not
/// Add a musician to a voice on a collection item.
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
    Json(body): Json<AssignRequest>,
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

/// Request body for `PATCH .../assignments/{assignmentId}` — sets
/// `notifiedAt`/`acknowledgedAt`. The notification/acknowledgement
/// *workflow* is deferred (CLAUDE.md phase-1 cut), but the fields are
/// settable now. Omitted fields are left unchanged.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateAssignmentStateRequest {
    notified_at: Option<DateTime<Utc>>,
    acknowledged_at: Option<DateTime<Utc>>,
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
    Json(body): Json<UpdateAssignmentStateRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current =
        find_assignment_scoped(&state, org_id, collection_id, item_id, assignment_id).await?;
    crate::listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let updated = part_assignment::update_state(
        &state.db,
        assignment_id,
        body.notified_at,
        body.acknowledged_at,
    )
    .await?
    .ok_or(AppError::NotFound)?;

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

    if !part_assignment::delete(&state.db, assignment_id).await? {
        return Err(AppError::NotFound);
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
