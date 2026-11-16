//! `/v1/orgs/{orgId}/collections/{collectionId}/sections` and
//! `…/label-choices` — issue #103 (sections of a standing collection).
//!
//! Reads follow the collection's read rule (#123: a restricted musician
//! reads only a collection holding their parts); writes require
//! `owner`/`archivist`/`conductor`, like every collection write
//! (docs/design.md Permission matrix: "Build/edit collections").

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::{require_collection_editor_v1, require_collection_reader_v1};
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::collection_item;
use crate::domain::collection_section::{self, CollectionSection, SectionChanges, SectionError};
use crate::error::AppError;
use crate::listing::{self, etag_response, etag_response_status, if_match_header};
use crate::routes::collections::{find_collection_scoped, item_error_to_app_error};
use crate::routes::json::ApiJson;
use crate::routes::openapi::{
    CommonErrors, Conflict409, Forbidden403, JsonBodyErrors, NotFound404, Precondition412,
    Validation400,
};
use crate::routes::patch::reject_blank;
use crate::routes::RequestId;
// Problem Details on a bad parameter (issue #66); see `routes::params`.
use crate::routes::params::ApiPath as Path;
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_sections, create_section))
        .routes(routes!(get_section, update_section, delete_section))
        .routes(routes!(label_choices))
}

fn section_error_to_app_error(err: SectionError) -> AppError {
    match err {
        SectionError::Database(e) => AppError::Database(e),
        SectionError::CollectionGone => AppError::NotFound,
        err @ SectionError::InvalidStart { .. } => AppError::field("startsAt", &err.to_string()),
        err @ (SectionError::NotStanding | SectionError::StartTaken { .. }) => {
            AppError::Conflict(err.to_string())
        }
    }
}

/// A section of the collection, or 404.
async fn find_section_scoped(
    state: &AppState,
    org_id: Uuid,
    collection_id: Uuid,
    section_id: Uuid,
) -> Result<CollectionSection, AppError> {
    find_collection_scoped(state, org_id, collection_id).await?;
    collection_section::find_by_id(&state.db, section_id)
        .await?
        .filter(|s| s.collection_id == collection_id)
        .ok_or(AppError::NotFound)
}

/// List a collection's sections by start (any reader of the collection). Not
/// paginated: a folder has a handful, and a client needs all of them to
/// place its pieces.
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/sections",
    tag = "collections",
    summary = "List collection sections",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Sections by start", body = Vec<CollectionSection>),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn list_sections(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<CollectionSection>>, AppError> {
    require_collection_reader_v1(&state, &auth, org_id, collection_id).await?;
    find_collection_scoped(&state, org_id, collection_id).await?;
    Ok(Json(
        collection_section::list_for_collection(&state.db, collection_id).await?,
    ))
}

/// Request body for adding a section.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateSectionRequest {
    /// "Märsche", "Polkas", "Modern".
    name: String,
    /// The first number the section covers, 1 up to the collection's
    /// width (999 or 9999). It may lie past the highest number (a new
    /// section after the others), or inside the numbers in use, dividing
    /// the folder; no number changes either way.
    starts_at: i32,
}

/// Add a section to a standing collection (owner/archivist/conductor).
///
/// A program has no sections (409); a start another section has is a 409.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/sections",
    tag = "collections",
    summary = "Add a collection section",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = CreateSectionRequest,
    responses(
        (status = 201, description = "Section added", body = CollectionSection,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        JsonBodyErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Conflict409,
    )
)]
async fn create_section(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<CreateSectionRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, collection_id).await?;
    reject_blank("name", Some(&body.name))?;

    let created = collection_section::create(
        &state.db,
        Uuid::now_v7(),
        collection_id,
        body.name.trim(),
        body.starts_at,
        Some(auth.user.id),
    )
    .await
    .map_err(section_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection_section.create",
        "collection_section",
        Some(created.id),
        serde_json::json!({
            "collectionId": collection_id,
            "name": created.name,
            "startsAt": created.starts_at,
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

/// Fetch one section (any reader of the collection).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/sections/{sectionId}",
    tag = "collections",
    summary = "Get a collection section",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
        ("sectionId"    = Uuid, Path, description = "Section ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Section", body = CollectionSection,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_section(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id, section_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_collection_reader_v1(&state, &auth, org_id, collection_id).await?;
    let section = find_section_scoped(&state, org_id, collection_id, section_id).await?;
    let updated_at = section.updated_at;
    Ok(etag_response(section, updated_at))
}

/// Request body for changing a section: a partial update like every
/// `PATCH`, so `{}` changes nothing.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateSectionRequest {
    name: Option<String>,
    /// A new first number. The pieces around it regroup; no number changes.
    starts_at: Option<i32>,
}

/// Rename a section or move its start (owner/archivist/conductor; requires
/// `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/collections/{collectionId}/sections/{sectionId}",
    tag = "collections",
    summary = "Update a collection section",
    params(
        ("orgId"        = Uuid,   Path,   description = "Organization ID"),
        ("collectionId" = Uuid,   Path,   description = "Collection ID"),
        ("sectionId"    = Uuid,   Path,   description = "Section ID"),
        ("If-Match"     = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateSectionRequest,
    responses(
        (status = 200, description = "Updated section", body = CollectionSection,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        JsonBodyErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Conflict409,
        Precondition412,
    )
)]
async fn update_section(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, section_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<UpdateSectionRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current = find_section_scoped(&state, org_id, collection_id, section_id).await?;
    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;
    reject_blank("name", body.name.as_deref())?;

    let Some(updated) = collection_section::update(
        &state.db,
        collection_id,
        section_id,
        Some(listing::version_of(current.updated_at)),
        SectionChanges {
            name: body.name.as_deref().map(str::trim),
            starts_at: body.starts_at,
        },
    )
    .await
    .map_err(section_error_to_app_error)?
    else {
        return Err(listing::stale_or_gone(find_section_scoped(
            &state,
            org_id,
            collection_id,
            section_id,
        ))
        .await);
    };
    if updated.updated_at == current.updated_at {
        // Nothing changed: nothing written, nothing audited.
        let updated_at = updated.updated_at;
        return Ok(etag_response(updated, updated_at));
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection_section.update",
        "collection_section",
        Some(section_id),
        serde_json::json!({
            "before": { "name": current.name, "startsAt": current.starts_at },
            "after": { "name": updated.name, "startsAt": updated.starts_at },
        }),
    )
    .await;

    let updated_at = updated.updated_at;
    Ok(etag_response(updated, updated_at))
}

/// Delete a section (owner/archivist/conductor; requires `If-Match`). Only
/// the heading goes: its pieces fall into the section before it, and no
/// number changes.
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/collections/{collectionId}/sections/{sectionId}",
    tag = "collections",
    summary = "Delete a collection section",
    params(
        ("orgId"        = Uuid,   Path,   description = "Organization ID"),
        ("collectionId" = Uuid,   Path,   description = "Collection ID"),
        ("sectionId"    = Uuid,   Path,   description = "Section ID"),
        ("If-Match"     = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Section deleted"),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_section(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, section_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current = find_section_scoped(&state, org_id, collection_id, section_id).await?;
    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if !collection_section::delete(
        &state.db,
        collection_id,
        section_id,
        Some(listing::version_of(current.updated_at)),
    )
    .await
    .map_err(section_error_to_app_error)?
    {
        return Err(listing::stale_or_gone(find_section_scoped(
            &state,
            org_id,
            collection_id,
            section_id,
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
        "collection_section.delete",
        "collection_section",
        Some(section_id),
        serde_json::json!({
            "collectionId": collection_id,
            "name": current.name,
            "startsAt": current.starts_at,
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// The labels a new piece of a standing collection can take.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct LabelChoices {
    /// The highest number + 1 (`1` in an empty collection): the default
    /// when a piece is added without a label. `null` when it would not fit
    /// the collection's width.
    suggested: Option<String>,
    /// Free numbers below the highest one of their section, ascending.
    gaps: Vec<String>,
    /// Each section's next number, by start; empty without sections. The
    /// numbers before the first section appear as an entry without a
    /// section when the first section does not start at 1.
    sections: Vec<SectionLabelChoice>,
}

/// The next number one section offers.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct SectionLabelChoice {
    /// `null` for the numbers before the first section.
    section_id: Option<Uuid>,
    name: Option<String>,
    starts_at: i32,
    /// The section's start while it holds no piece, else the number after
    /// its highest. `null` when the section is full (that number is the
    /// next section's start) or past the collection's width.
    next: Option<String>,
    /// `true` when `next` is `null`: take a gap, a letter label (`75a`), or
    /// renumber.
    full: bool,
}

/// The labels a new piece of a standing collection can take (requires org
/// membership): the suggestion, the gaps, and each section's next number.
/// Letter labels (`12a`) after any number in use are allowed too and are not
/// listed. A program has no labels to choose (409).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/label-choices",
    tag = "collections",
    summary = "Label choices for a new piece",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "The allowed labels", body = LabelChoices),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Conflict409,
    )
)]
async fn label_choices(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<LabelChoices>, AppError> {
    require_collection_reader_v1(&state, &auth, org_id, collection_id).await?;
    find_collection_scoped(&state, org_id, collection_id).await?;
    let choices = collection_item::label_choices(&state.db, collection_id)
        .await
        .map_err(item_error_to_app_error)?;
    Ok(Json(LabelChoices {
        suggested: choices.suggested.map(|l| l.to_string()),
        gaps: choices.gaps.iter().map(ToString::to_string).collect(),
        sections: choices
            .sections
            .into_iter()
            .map(|c| SectionLabelChoice {
                section_id: c.section.as_ref().map(|s| s.id),
                name: c.section.map(|s| s.name),
                starts_at: i32::from(c.starts_at),
                full: c.next.is_none(),
                next: c.next.map(|l| l.to_string()),
            })
            .collect(),
    }))
}
