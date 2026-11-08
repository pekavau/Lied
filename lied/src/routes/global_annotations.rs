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

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::require_org_capability_v1;
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::global_annotation::{self, GlobalAnnotation, GlobalAnnotationError};
use crate::domain::membership::Role;
use crate::error::AppError;
use crate::listing::{self, SortDirection};
use crate::pagination::Page;
use crate::routes::arrangements::find_arrangement_scoped;
use crate::routes::openapi::{
    CommonErrors, Forbidden403, NotFound404, Precondition412, Validation400,
};
use crate::routes::RequestId;
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

fn etag_response<T: Serialize>(
    status: StatusCode,
    body: T,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> axum::response::Response {
    let mut response = Json(body).into_response();
    *response.status_mut() = status;
    if let Ok(value) = axum::http::HeaderValue::from_str(&listing::etag_for(updated_at)) {
        response
            .headers_mut()
            .insert(axum::http::header::ETAG, value);
    }
    response
}

fn if_match(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
}

fn domain_error(err: GlobalAnnotationError) -> AppError {
    match err {
        GlobalAnnotationError::InvalidType => AppError::field(
            "type",
            "type must be one of tempo, dynamics, articulation, structure, general",
        ),
        GlobalAnnotationError::EmptyContent => {
            AppError::field("content", "content must not be empty")
        }
        GlobalAnnotationError::ArrangementGone => AppError::NotFound,
        GlobalAnnotationError::Database(e) => AppError::Database(e),
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
            description = "Allowed: `createdAt` (default asc), `updatedAt`, `type`."),
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
        ("g.created_at", SortDirection::Asc),
    )
    .map_err(AppError::Validation)?;
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
            "type must be one of tempo, dynamics, articulation, structure, general",
        ));
    }

    let (items, total) = global_annotation::list_for_arrangement(
        &state.db,
        arrangement_id,
        i64::from(limit),
        i64::from(offset),
        sort_column,
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
#[serde(rename_all = "camelCase")]
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
    Json(body): Json<CreateRequest>,
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
    Ok(etag_response(StatusCode::CREATED, created, updated_at))
}

/// Fetch one global annotation (owner/archivist/conductor).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/annotations/{id}",
    tag = "global-annotations",
    summary = "Get a global annotation",
    params(
        ("orgId"         = Uuid, Path, description = "Organization ID"),
        ("arrangementId" = Uuid, Path, description = "Arrangement ID"),
        ("id"            = Uuid, Path, description = "Annotation ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Annotation", body = GlobalAnnotation,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
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
    Ok(etag_response(StatusCode::OK, found, updated_at))
}

/// Request body for `PATCH …/annotations/{id}`: every field optional, and an
/// omitted one is left unchanged.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateRequest {
    #[serde(rename = "type")]
    annotation_type: Option<String>,
    content: Option<String>,
}

/// Change a global annotation's type and/or content (owner/conductor;
/// requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/annotations/{id}",
    tag = "global-annotations",
    summary = "Update a global annotation",
    params(
        ("orgId"         = Uuid,   Path,   description = "Organization ID"),
        ("arrangementId" = Uuid,   Path,   description = "Arrangement ID"),
        ("id"            = Uuid,   Path,   description = "Annotation ID"),
        ("If-Match"      = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateRequest,
    responses(
        (status = 200, description = "Updated annotation", body = GlobalAnnotation,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
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
    Json(body): Json<UpdateRequest>,
) -> Result<axum::response::Response, AppError> {
    require_org_capability_v1(&state, &auth, org_id, Role::can_author_global_annotations).await?;
    let current = find_scoped(&state, org_id, arrangement_id, id).await?;
    listing::check_if_match(if_match(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let updated = global_annotation::update(
        &state.db,
        id,
        body.annotation_type.as_deref(),
        body.content.as_deref(),
    )
    .await
    .map_err(domain_error)?
    .ok_or(AppError::NotFound)?;

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
    Ok(etag_response(StatusCode::OK, updated, updated_at))
}

/// Delete a global annotation for good (owner/conductor; requires `If-Match`).
/// There is no undelete: the audit log keeps the removed content.
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/annotations/{id}",
    tag = "global-annotations",
    summary = "Delete a global annotation (hard delete)",
    params(
        ("orgId"         = Uuid,   Path,   description = "Organization ID"),
        ("arrangementId" = Uuid,   Path,   description = "Arrangement ID"),
        ("id"            = Uuid,   Path,   description = "Annotation ID"),
        ("If-Match"      = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Annotation deleted"),
        CommonErrors,
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
    listing::check_if_match(if_match(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if !global_annotation::delete(&state.db, id).await? {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &ctx(&auth, org_id, request_id),
        "global_annotation.delete",
        "global_annotation",
        Some(id),
        serde_json::json!({
            "hardDelete": true,
            "arrangementId": arrangement_id,
            "authorId": current.author_id,
            "type": current.annotation_type,
            "content": current.content,
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}
