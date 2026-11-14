//! `/v1/orgs/{orgId}/arrangements/{arrangementId}/annotations` — a conductor's
//! global annotations on an arrangement (issue #36, UC-16 authoring).
//!
//! Permission matrix: **owner and conductor** write; **staff** (owner,
//! archivist, conductor) read. A plain musician gets 403 for now: annotations
//! reach musicians through the phase-3 client, and whether a musician may read
//! them over `/v1` is decided there, not by default here.
//!
//! Writes follow the API guidelines: ETag on every annotation response,
//! `If-Match` required on `PATCH`/`DELETE` (stale → 412), RFC 7807 errors, an
//! audit row per write. `PATCH` is a true partial update: an omitted field is
//! left as it is. `DELETE` is a **hard** delete (no `deleted_at` on this
//! entity), so its audit row keeps the deleted content — afterwards it is the
//! only copy.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::require_org_capability_v1;
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::global_annotation::{self, GlobalAnnotation, GlobalAnnotationError, WriteError};
use crate::domain::membership::Role;
use crate::error::AppError;
use crate::listing::{self, etag_response, etag_response_status, if_match_header};
use crate::pagination::Page;
use crate::routes::arrangements::find_arrangement_scoped;
use crate::routes::json::ApiJson;
use crate::routes::openapi::{
    CommonErrors, Forbidden403, JsonBodyErrors, NotFound404, Precondition412, Validation400,
};
use crate::routes::RequestId;
// Problem Details on a bad parameter (issue #66); see `routes::params`.
use crate::routes::params::{ApiPath as Path, ApiQuery as Query};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_annotations, create_annotation))
        .routes(routes!(
            get_annotation,
            update_annotation,
            delete_annotation
        ))
}

fn domain_error(err: GlobalAnnotationError) -> AppError {
    match err {
        GlobalAnnotationError::InvalidType => {
            AppError::field("type", &global_annotation::invalid_type_message())
        }
        GlobalAnnotationError::EmptyContent => {
            AppError::field("content", "content must not be empty")
        }
        GlobalAnnotationError::ArrangementGone => AppError::NotFound,
        GlobalAnnotationError::Database(e) => AppError::Database(e),
    }
}

fn write_error(err: WriteError) -> AppError {
    match err {
        WriteError::Invalid(e) => domain_error(e),
        WriteError::Stale => AppError::PreconditionFailed,
        WriteError::NotFound => AppError::NotFound,
    }
}

/// The version `If-Match` names, if it names the current one. A missing,
/// unparseable or stale header is a 412 (CLAUDE.md: PATCH/DELETE MUST send
/// `If-Match`). The write re-checks the same version atomically, so this early
/// answer is a courtesy, not the guarantee.
fn expected_version(headers: &HeaderMap, current: &GlobalAnnotation) -> Result<i64, AppError> {
    match listing::parse_if_match(if_match_header(headers)) {
        Some(version) if version == current.updated_at.timestamp_millis() => Ok(version),
        _ => Err(AppError::PreconditionFailed),
    }
}

fn ctx(auth: &BearerOrSession, org_id: Uuid, request_id: Uuid) -> AuditContext {
    AuditContext {
        actor_user_id: Some(auth.user.id),
        org_id: Some(org_id),
        request_id: Some(request_id),
    }
}

async fn find_scoped(
    state: &AppState,
    org_id: Uuid,
    arrangement_id: Uuid,
    id: Uuid,
) -> Result<GlobalAnnotation, AppError> {
    global_annotation::find(&state.db, org_id, arrangement_id, id)
        .await?
        .ok_or(AppError::NotFound)
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    sort: Option<String>,
}

/// List an arrangement's global annotations (owner/archivist/conductor).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/annotations",
    tag = "global-annotations",
    summary = "List global annotations",
    params(
        ("orgId"         = Uuid, Path, description = "Organization ID"),
        ("arrangementId" = Uuid, Path, description = "Arrangement ID"),
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort as `field`, `field:asc` or `field:desc` (a bare field sorts ascending). Allowed fields: `createdAt` (default, ascending), `updatedAt`, `type`."),
        ("filter[type]" = Option<String>, Query,
            description = "One of `tempo`, `dynamics`, `articulation`, `structure`, `general`"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated annotation list",
            body = inline(Page<GlobalAnnotation>)),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Validation400,
    )
)]
async fn list_annotations(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arrangement_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<ListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<GlobalAnnotation>>, AppError> {
    require_org_capability_v1(&state, &auth, org_id, Role::is_staff).await?;
    find_arrangement_scoped(&state, org_id, arrangement_id).await?;

    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);
    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        global_annotation::SORT_ALLOWLIST,
        global_annotation::DEFAULT_SORT,
    )
    .map_err(AppError::Validation)?;
    let sort = global_annotation::SortField::from_key(sort_column)
        .unwrap_or(global_annotation::SortField::CreatedAt);
    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters = listing::resolve_filters(&raw_filters, global_annotation::FILTER_ALLOWLIST)
        .map_err(AppError::Validation)?;
    let type_filter = filters
        .iter()
        .find(|(field, _)| field == "type")
        .map(|(_, value)| value.as_str());
    if type_filter.is_some_and(|t| !global_annotation::is_valid_type(t)) {
        return Err(AppError::field(
            "filter[type]",
            &global_annotation::invalid_type_message(),
        ));
    }

    let (items, total) = global_annotation::list_for_arrangement(
        &state.db,
        arrangement_id,
        i64::from(limit),
        i64::from(offset),
        sort,
        sort_direction,
        type_filter,
    )
    .await?;
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Request body for `POST …/annotations`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateRequest {
    /// One of `tempo`, `dynamics`, `articulation`, `structure`, `general`.
    #[serde(rename = "type")]
    annotation_type: String,
    /// The annotation itself, e.g. "bar 112: cut to the coda".
    content: String,
}

/// Add a global annotation to an arrangement (owner/conductor). The caller is
/// recorded as its author.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/annotations",
    tag = "global-annotations",
    summary = "Create a global annotation",
    params(
        ("orgId"         = Uuid, Path, description = "Organization ID"),
        ("arrangementId" = Uuid, Path, description = "Arrangement ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = CreateRequest,
    responses(
        (status = 201, description = "Annotation created", body = GlobalAnnotation,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        JsonBodyErrors,
        Forbidden403,
        NotFound404,
        Validation400,
    )
)]
async fn create_annotation(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arrangement_id)): Path<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<CreateRequest>,
) -> Result<axum::response::Response, AppError> {
    require_org_capability_v1(&state, &auth, org_id, Role::can_author_global_annotations).await?;
    find_arrangement_scoped(&state, org_id, arrangement_id).await?;

    let created = global_annotation::create(
        &state.db,
        Uuid::now_v7(),
        arrangement_id,
        auth.user.id,
        &body.annotation_type,
        &body.content,
    )
    .await
    .map_err(domain_error)?;

    audit(
        &state.db,
        &ctx(&auth, org_id, request_id),
        "global_annotation.create",
        "global_annotation",
        Some(created.id),
        serde_json::json!({
            "arrangementId": arrangement_id,
            "type": created.annotation_type,
            "content": created.content,
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

/// Fetch one global annotation (owner/archivist/conductor).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/annotations/{annotationId}",
    tag = "global-annotations",
    summary = "Get a global annotation",
    params(
        ("orgId"         = Uuid, Path, description = "Organization ID"),
        ("arrangementId" = Uuid, Path, description = "Arrangement ID"),
        ("annotationId"            = Uuid, Path, description = "Annotation ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Annotation", body = GlobalAnnotation,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_annotation(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arrangement_id, id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_org_capability_v1(&state, &auth, org_id, Role::is_staff).await?;
    let found = find_scoped(&state, org_id, arrangement_id, id).await?;
    let updated_at = found.updated_at;
    Ok(etag_response(found, updated_at))
}

/// Request body for `PATCH …/annotations/{id}`: every field optional, and an
/// omitted one is left unchanged. Unknown fields are refused, so a misspelled
/// key is a 400 rather than a silent no-op.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateRequest {
    #[serde(rename = "type")]
    annotation_type: Option<String>,
    content: Option<String>,
}

/// Change a global annotation's type and/or content (owner/conductor;
/// requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/annotations/{annotationId}",
    tag = "global-annotations",
    summary = "Update a global annotation",
    params(
        ("orgId"         = Uuid,   Path,   description = "Organization ID"),
        ("arrangementId" = Uuid,   Path,   description = "Arrangement ID"),
        ("annotationId"            = Uuid,   Path,   description = "Annotation ID"),
        ("If-Match"      = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateRequest,
    responses(
        (status = 200, description = "Updated annotation", body = GlobalAnnotation,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        JsonBodyErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Precondition412,
    )
)]
async fn update_annotation(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arrangement_id, id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<UpdateRequest>,
) -> Result<axum::response::Response, AppError> {
    require_org_capability_v1(&state, &auth, org_id, Role::can_author_global_annotations).await?;
    let current = find_scoped(&state, org_id, arrangement_id, id).await?;
    let expected = expected_version(&headers, &current)?;

    // Nothing to change (no fields, or the same values): answer with the
    // current row and write nothing — no new version, no audit row, no 412 for
    // everyone else holding the current ETag.
    let changes_type = body
        .annotation_type
        .as_deref()
        .is_some_and(|t| t != current.annotation_type);
    let changes_content = body
        .content
        .as_deref()
        .is_some_and(|c| c != current.content);
    if !changes_type && !changes_content {
        // Still validate what was sent: an invalid value is an error even if
        // it would not change anything.
        if body
            .annotation_type
            .as_deref()
            .is_some_and(|t| !global_annotation::is_valid_type(t))
        {
            return Err(domain_error(GlobalAnnotationError::InvalidType));
        }
        let updated_at = current.updated_at;
        return Ok(etag_response(current, updated_at));
    }

    let updated = global_annotation::update(
        &state.db,
        id,
        expected,
        body.annotation_type.as_deref(),
        body.content.as_deref(),
    )
    .await
    .map_err(write_error)?;

    audit(
        &state.db,
        &ctx(&auth, org_id, request_id),
        "global_annotation.update",
        "global_annotation",
        Some(id),
        serde_json::json!({
            "arrangementId": arrangement_id,
            "before": { "type": current.annotation_type, "content": current.content },
            "after": { "type": updated.annotation_type, "content": updated.content },
        }),
    )
    .await;

    let updated_at = updated.updated_at;
    Ok(etag_response(updated, updated_at))
}

/// Delete a global annotation for good (owner/conductor; requires `If-Match`).
/// There is no undelete: the audit log keeps the removed content.
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/annotations/{annotationId}",
    tag = "global-annotations",
    summary = "Delete a global annotation (hard delete)",
    params(
        ("orgId"         = Uuid,   Path,   description = "Organization ID"),
        ("arrangementId" = Uuid,   Path,   description = "Arrangement ID"),
        ("annotationId"            = Uuid,   Path,   description = "Annotation ID"),
        ("If-Match"      = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Annotation deleted"),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_annotation(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arrangement_id, id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_org_capability_v1(&state, &auth, org_id, Role::can_author_global_annotations).await?;
    let current = find_scoped(&state, org_id, arrangement_id, id).await?;
    let expected = expected_version(&headers, &current)?;
    // What the write removed, not what was read before it: the audit row is
    // the only copy left.
    let removed = global_annotation::delete(&state.db, id, expected)
        .await
        .map_err(write_error)?;

    audit(
        &state.db,
        &ctx(&auth, org_id, request_id),
        "global_annotation.delete",
        "global_annotation",
        Some(id),
        serde_json::json!({
            "hardDelete": true,
            "arrangementId": arrangement_id,
            "authorId": removed.author_id,
            "type": removed.annotation_type,
            "content": removed.content,
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}
