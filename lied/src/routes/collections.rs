//! `/v1/orgs/{orgId}/collections` and `.../collections/{collectionId}/items` — issue #9
//! (Collections + CollectionItems: program assembly and standing repertoire).
//!
//! Reads require org membership and follow the reader's read scope (#123: a
//! restricted musician sees only the collections and items holding their
//! parts); writes (build/edit) require
//! `owner`/`archivist`/`conductor` (docs/design.md Permission matrix), enforced via
//! [`require_collection_editor_v1`]. Optimistic concurrency (ETag/`If-Match`),
//! RFC 7807 errors, and an `audit()` on every write, per the API guidelines.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::{
    require_archive_lister_v1, require_collection_editor_v1, require_collection_item_reader_v1,
    require_collection_reader_v1, require_coverage_viewer_v1, CoverageScope,
};
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::{collection, collection_item, coverage};
use crate::error::AppError;
use crate::listing::{self};
use crate::listing::{etag_response, etag_response_status, if_match_header};
use crate::pagination::Page;
use crate::routes::json::{ApiJson, OptionalApiJson};
use crate::routes::openapi::{
    CommonErrors, Conflict409, Forbidden403, JsonBodyErrors, NotFound404, Precondition412,
    Validation400,
};
use crate::routes::patch::reject_blank;
use crate::routes::RequestId;
// Problem Details on a bad parameter (issue #66); see `routes::params`.
use crate::routes::params::{ApiListQuery, ApiPath as Path};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_collections, create_collection))
        .routes(routes!(
            get_collection,
            update_collection,
            delete_collection
        ))
        .routes(routes!(undelete_collection))
        .routes(routes!(reorder_items))
        .routes(routes!(preview_renumber, renumber_collection))
        .routes(routes!(list_items, add_item))
        .routes(routes!(get_item, update_item, delete_item))
        .routes(routes!(undelete_item))
        .routes(routes!(swap_item))
        .routes(routes!(collection_coverage))
}

// ── shared helpers ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    sort: Option<String>,
}

fn collection_error_to_app_error(err: collection::CollectionError) -> AppError {
    match err {
        collection::CollectionError::DuplicateSlug => {
            AppError::Conflict("a collection with this name already exists".into())
        }
        err @ (collection::CollectionError::LabelsTooWide { .. }
        | collection::CollectionError::SectionsTooWide { .. }
        | collection::CollectionError::HasSections) => AppError::Conflict(err.to_string()),
        collection::CollectionError::Database(e) => AppError::Database(e),
    }
}

fn validate_number_width(width: Option<i16>) -> Result<(), AppError> {
    match width {
        Some(width) if !collection::is_valid_number_width(width) => {
            Err(AppError::field("numberWidth", "numberWidth must be 3 or 4"))
        }
        _ => Ok(()),
    }
}

/// How a numbering refusal reaches the client: a field error (400) when the
/// request itself is wrong, a conflict (409) when the collection's current
/// state stands in the way.
pub(crate) fn item_error_to_app_error(err: collection_item::CollectionItemError) -> AppError {
    use collection_item::{CollectionItemError as E, LabelRefusal};
    match err {
        E::Database(e) => AppError::Database(e),
        E::CollectionGone => AppError::NotFound,
        E::UnknownReference => {
            AppError::Conflict("the referenced arrangement does not exist".into())
        }
        E::InvalidReorder => {
            AppError::Conflict("orderedIds must be exactly the collection's current items".into())
        }
        E::DuplicateArrangement => {
            AppError::Conflict("this arrangement is already an item in the collection".into())
        }
        err @ E::WrongField { field, .. } => AppError::field(field, &err.to_string()),
        err @ E::InvalidPosition { .. } => AppError::field("position", &err.to_string()),
        err @ E::InvalidLabel => AppError::field("label", &err.to_string()),
        err @ E::LabelRefused {
            refusal: LabelRefusal::Taken,
            ..
        } => AppError::Conflict(err.to_string()),
        err @ E::LabelRefused { .. } => AppError::field("label", &err.to_string()),
        err @ E::SameItem => AppError::field("withItemId", &err.to_string()),
        err @ (E::LabelNeeded { .. }
        | E::CollectionFull
        | E::NotStanding
        | E::NotProgram
        | E::StaleRenumber) => AppError::Conflict(err.to_string()),
    }
}

pub(crate) async fn find_collection_scoped(
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

async fn find_collection_scoped_including_deleted(
    state: &AppState,
    org_id: Uuid,
    id: Uuid,
) -> Result<collection::Collection, AppError> {
    let found = collection::find_by_id_including_deleted(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.organization_id != org_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

/// Resolve an item, verifying the collection belongs to the org and the item
/// belongs to the collection. `include_deleted` covers the undelete flow.
async fn find_item_scoped(
    state: &AppState,
    org_id: Uuid,
    collection_id: Uuid,
    item_id: Uuid,
    include_deleted: bool,
) -> Result<collection_item::CollectionItem, AppError> {
    // The parent collection must exist in the org (live for normal ops).
    find_collection_scoped(state, org_id, collection_id).await?;
    let found = if include_deleted {
        collection_item::find_by_id_including_deleted(&state.db, item_id).await?
    } else {
        collection_item::find_by_id(&state.db, item_id).await?
    }
    .ok_or(AppError::NotFound)?;
    if found.collection_id != collection_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

// ── Collection ─────────────────────────────────────────────────────────────

/// Request body for creating a collection.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateCollectionRequest {
    name: String,
    /// `program` (a concert: numbers are positions) or `standing` (a
    /// Marschbuch: numbers are labels that keep their gaps).
    #[serde(rename = "type")]
    collection_type: String,
    /// Digits in the WebDAV folder names, and so the highest label: `3`
    /// (up to 999, the default) or `4` (up to 9999, for a large book).
    #[serde(default)]
    number_width: Option<i16>,
}

/// Request body for `PATCH …/collections/{collectionId}`: a partial update; an omitted
/// field is kept.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateCollectionRequest {
    name: Option<String>,
    /// `program` or `standing`. Program → standing keeps the numbers as
    /// labels; standing → program closes the gaps.
    #[serde(rename = "type")]
    collection_type: Option<String>,
    /// `3` or `4`. Widening renames every WebDAV folder; narrowing is a 409
    /// while a label would not fit.
    number_width: Option<i16>,
}

/// List an organization's collections (members; a restricted musician sees
/// only those holding their parts).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections",
    tag = "collections",
    summary = "List collections",
    params(
        ("orgId"  = Uuid,           Path,  description = "Organization ID"),
        ("limit"  = Option<u32>,    Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>,    Query, description = "Page offset"),
        ("sort"   = Option<String>, Query, 
            description = "Sort as `field`, `field:asc` or `field:desc` (a bare field sorts ascending). Allowed fields: `name` (default, ascending), `createdAt`."),
        ("filter[type]" = Option<String>, Query, description = "Filter by type: `program` or `standing`"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated collection list",
            body = inline(Page<collection::Collection>)),
        CommonErrors,
        Forbidden403,
        Validation400,
    )
)]
async fn list_collections(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path(org_id): Path<Uuid>,
    ApiListQuery {
        params: q,
        filters: raw_filters,
    }: ApiListQuery<ListQuery>,
) -> Result<Json<Page<collection::Collection>>, AppError> {
    // A restricted musician lists only the collections they hold a part in
    // (#123), and the type filter and total apply within that set, as the
    // WebDAV collections tree shows them.
    let scope = require_archive_lister_v1(&state, &auth, org_id).await?;
    let assigned_to = (!scope.reads_everything()).then_some(auth.user.id);

    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        collection::SORT_ALLOWLIST,
        collection::DEFAULT_SORT,
    )
    .map_err(AppError::Validation)?;
    let filters = listing::resolve_filters(&raw_filters, collection::FILTER_ALLOWLIST)
        .map_err(AppError::Validation)?;
    let type_filter = filters
        .iter()
        .find(|(field, _)| field == "type")
        .map(|(_, value)| value.as_str());

    let (items, total) = collection::list_for_org(
        &state.db,
        org_id,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        type_filter,
        assigned_to,
    )
    .await?;

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Create a collection (owner/archivist/conductor).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections",
    tag = "collections",
    summary = "Create a collection",
    params(("orgId" = Uuid, Path, description = "Organization ID")),
    security(("bearer" = []), ("session" = [])),
    request_body = CreateCollectionRequest,
    responses(
        (status = 201, description = "Collection created", body = collection::Collection,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        JsonBodyErrors,
        Forbidden403,
        Validation400,
        Conflict409,
    )
)]
async fn create_collection(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(org_id): Path<Uuid>,
    ApiJson(body): ApiJson<CreateCollectionRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;

    if body.name.trim().is_empty() {
        return Err(AppError::field("name", "name must not be empty"));
    }
    if !collection::is_valid_type(&body.collection_type) {
        return Err(AppError::field(
            "type",
            "type must be `program` or `standing`",
        ));
    }
    validate_number_width(body.number_width)?;

    let id = Uuid::now_v7();
    let slug = collection::slugify(&body.name);
    let created = collection::create(
        &state.db,
        id,
        org_id,
        &body.name,
        &slug,
        &body.collection_type,
        body.number_width.unwrap_or(3),
        Some(auth.user.id),
    )
    .await
    .map_err(collection_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection.create",
        "collection",
        Some(created.id),
        serde_json::json!({
            "name": created.name,
            "slug": created.slug,
            "type": created.collection_type,
            "numberWidth": created.number_width,
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

/// Fetch one collection (members; a restricted musician only one holding
/// their parts, else 403).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}",
    tag = "collections",
    summary = "Get a collection",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Collection", body = collection::Collection,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_collection(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_collection_reader_v1(&state, &auth, org_id, id).await?;
    let found = find_collection_scoped(&state, org_id, id).await?;
    let updated_at = found.updated_at;
    Ok(etag_response(found, updated_at))
}

/// Update a collection's name, type or number width (owner/archivist/conductor;
/// requires `If-Match`).
///
/// Changing the type from `standing` to `program` closes the gaps in the
/// numbering (a program's labels are its positions). Narrowing `numberWidth`
/// is a 409 while a piece's label would not fit.
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/collections/{collectionId}",
    tag = "collections",
    summary = "Update a collection",
    params(
        ("orgId"        = Uuid,   Path,   description = "Organization ID"),
        ("collectionId" = Uuid,   Path,   description = "Collection ID"),
        ("If-Match"     = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateCollectionRequest,
    responses(
        (status = 200, description = "Updated collection", body = collection::Collection,
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
async fn update_collection(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<UpdateCollectionRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;

    let current = find_collection_scoped(&state, org_id, id).await?;
    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    reject_blank("name", body.name.as_deref())?;
    if body
        .collection_type
        .as_deref()
        .is_some_and(|t| !collection::is_valid_type(t))
    {
        return Err(AppError::field(
            "type",
            "type must be `program` or `standing`",
        ));
    }
    validate_number_width(body.number_width)?;
    let name = body.name.unwrap_or_else(|| current.name.clone());
    let collection_type = body
        .collection_type
        .unwrap_or_else(|| current.collection_type.clone());
    let number_width = body.number_width.unwrap_or(current.number_width);
    if name == current.name
        && collection_type == current.collection_type
        && number_width == current.number_width
    {
        let updated_at = current.updated_at;
        return Ok(etag_response(current, updated_at));
    }

    let Some(updated) = collection::update(
        &state.db,
        id,
        Some(listing::version_of(current.updated_at)),
        collection::CollectionFields {
            name: &name,
            collection_type: &collection_type,
            number_width,
        },
    )
    .await
    .map_err(collection_error_to_app_error)?
    else {
        return Err(listing::stale_or_gone(find_collection_scoped(&state, org_id, id)).await);
    };

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection.update",
        "collection",
        Some(id),
        serde_json::json!({
            "before": {
                "name": current.name,
                "type": current.collection_type,
                "numberWidth": current.number_width,
            },
            "after": {
                "name": updated.name,
                "type": updated.collection_type,
                "numberWidth": updated.number_width,
            },
        }),
    )
    .await;

    let updated_at = updated.updated_at;
    Ok(etag_response(updated, updated_at))
}

/// Soft-delete a collection (owner/archivist/conductor; requires `If-Match`).
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/collections/{collectionId}",
    tag = "collections",
    summary = "Soft-delete a collection",
    params(
        ("orgId"        = Uuid,   Path,   description = "Organization ID"),
        ("collectionId" = Uuid,   Path,   description = "Collection ID"),
        ("If-Match"     = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Collection soft-deleted"),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_collection(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;

    let current = find_collection_scoped(&state, org_id, id).await?;
    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if !collection::soft_delete(&state.db, id, Some(listing::version_of(current.updated_at)))
        .await?
    {
        return Err(listing::stale_or_gone(find_collection_scoped(&state, org_id, id)).await);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection.soft_delete",
        "collection",
        Some(id),
        serde_json::json!({ "name": current.name, "slug": current.slug }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Restore a soft-deleted collection (owner/archivist/conductor).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/undelete",
    tag = "collections",
    summary = "Undelete a collection",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Collection restored"),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Conflict409,
    )
)]
async fn undelete_collection(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;

    let current = find_collection_scoped_including_deleted(&state, org_id, id).await?;
    if !collection::undelete(&state.db, id)
        .await
        .map_err(collection_error_to_app_error)?
    {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection.undelete",
        "collection",
        Some(id),
        serde_json::json!({ "name": current.name, "slug": current.slug }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Request body for reordering a collection's items.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReorderRequest {
    /// The item IDs in their new order; reassigned positions `1..=n`.
    ordered_ids: Vec<Uuid>,
}

/// Reorder a program's items (owner/archivist/conductor).
///
/// Programs only: a standing collection's order follows its labels, so it
/// answers 409; relabel, swap or renumber its pieces instead.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/reorder",
    tag = "collections",
    summary = "Reorder collection items",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = ReorderRequest,
    responses(
        (status = 204, description = "Items reordered"),
        CommonErrors,
        JsonBodyErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Conflict409,
    )
)]
async fn reorder_items(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<ReorderRequest>,
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, id).await?;

    collection_item::reorder(&state.db, id, &body.ordered_ids)
        .await
        .map_err(item_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection.reorder",
        "collection",
        Some(id),
        serde_json::json!({ "orderedIds": body.ordered_ids }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ── CollectionItem ─────────────────────────────────────────────────────────

/// List a collection's items in position order (members; a restricted
/// musician sees only the items they play).
/// Display each item's `label`. A soft-deleted arrangement is surfaced with
/// `arrangementRemoved: true` and its slug, not dropped (hide-with-references).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/items",
    tag = "collections",
    summary = "List collection items",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Items in position order",
            body = Vec<collection_item::CollectionItemView>),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn list_items(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<collection_item::CollectionItemView>>, AppError> {
    // A restricted musician sees only the items they hold a part on (#123).
    let scope = require_collection_reader_v1(&state, &auth, org_id, id).await?;
    find_collection_scoped(&state, org_id, id).await?;
    let assigned_to = (!scope.reads_everything()).then_some(auth.user.id);
    let items = collection_item::list_for_collection(&state.db, id, assigned_to).await?;
    Ok(Json(items))
}

/// Request body for adding an item to a collection.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateItemRequest {
    arrangement_id: Uuid,
    /// Programs only: where the piece goes, 1..n+1 (default: the end); the
    /// pieces from there on move down one.
    position: Option<i32>,
    /// Standing collections only: the piece's number, `12` or `12a`. Must
    /// be a free number below the highest, the highest + 1 (the default),
    /// or a letter after a number in use; anything else is a 400, a taken
    /// label a 409.
    label: Option<String>,
}

/// Add an arrangement to a collection (owner/archivist/conductor).
///
/// A program takes an optional `position`, a standing collection an optional
/// `label`; sending the other one is a 400 naming it.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/items",
    tag = "collections",
    summary = "Add a collection item",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = CreateItemRequest,
    responses(
        (status = 201, description = "Item added", body = collection_item::CollectionItem,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        JsonBodyErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Conflict409,
    )
)]
async fn add_item(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<CreateItemRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, id).await?;

    // The arrangement must live in the same org (else 404, not a leaked FK).
    let arr = crate::domain::arrangement::find_by_id(&state.db, body.arrangement_id).await?;
    match arr {
        Some(a) if a.organization_id == org_id => {}
        _ => return Err(AppError::NotFound),
    }

    let item_id = Uuid::now_v7();
    let created = collection_item::create(
        &state.db,
        item_id,
        id,
        body.arrangement_id,
        collection_item::Placement {
            position: body.position,
            label: body.label.as_deref(),
        },
        Some(auth.user.id),
    )
    .await
    .map_err(item_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection_item.create",
        "collection_item",
        Some(created.id),
        serde_json::json!({
            "collectionId": id,
            "arrangementId": body.arrangement_id,
            "position": created.position,
            "label": created.label,
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

/// Fetch one collection item (members; a restricted musician only one they
/// play, else 403).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}",
    tag = "collections",
    summary = "Get a collection item",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
        ("itemId"       = Uuid, Path, description = "Item ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Item", body = collection_item::CollectionItem,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_item(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_collection_item_reader_v1(&state, &auth, org_id, collection_id, item_id).await?;
    let item = find_item_scoped(&state, org_id, collection_id, item_id, false).await?;
    let updated_at = item.updated_at;
    Ok(etag_response(item, updated_at))
}

/// Request body for moving an item: a partial update like every `PATCH`, so
/// `{}` (or the item's current place) changes nothing.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateItemRequest {
    /// Programs only: the new position, 1..n; the pieces in between shift.
    position: Option<i32>,
    /// Standing collections only: the new label, under the same rule as
    /// adding (the piece's own current label does not count against it).
    label: Option<String>,
}

/// Move a collection item (owner/archivist/conductor; requires `If-Match`).
///
/// A program's item moves to a `position`; a standing collection's item to
/// a `label`. Nothing else in a standing collection changes number.
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}",
    tag = "collections",
    summary = "Move a collection item",
    params(
        ("orgId"        = Uuid,   Path,   description = "Organization ID"),
        ("collectionId" = Uuid,   Path,   description = "Collection ID"),
        ("itemId"       = Uuid,   Path,   description = "Item ID"),
        ("If-Match"     = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateItemRequest,
    responses(
        (status = 200, description = "Item moved", body = collection_item::CollectionItem,
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
async fn update_item(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<UpdateItemRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current = find_item_scoped(&state, org_id, collection_id, item_id, false).await?;
    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;
    if body.position.is_none() && body.label.is_none() {
        let updated_at = current.updated_at;
        return Ok(etag_response(current, updated_at));
    }

    let Some(updated) = collection_item::move_item(
        &state.db,
        collection_id,
        item_id,
        Some(listing::version_of(current.updated_at)),
        collection_item::Placement {
            position: body.position,
            label: body.label.as_deref(),
        },
    )
    .await
    .map_err(item_error_to_app_error)?
    else {
        return Err(listing::stale_or_gone(find_item_scoped(
            &state,
            org_id,
            collection_id,
            item_id,
            false,
        ))
        .await);
    };
    if updated.updated_at == current.updated_at {
        // Moved to where it already was: nothing written, nothing audited.
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
        "collection_item.update",
        "collection_item",
        Some(item_id),
        serde_json::json!({
            "before": { "position": current.position, "label": current.label },
            "after": { "position": updated.position, "label": updated.label },
        }),
    )
    .await;

    let updated_at = updated.updated_at;
    Ok(etag_response(updated, updated_at))
}

/// Remove a collection item (owner/archivist/conductor; requires `If-Match`).
///
/// In a program the gap closes; in a standing collection it stays, and the
/// other pieces keep their labels.
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}",
    tag = "collections",
    summary = "Soft-delete a collection item",
    params(
        ("orgId"        = Uuid,   Path,   description = "Organization ID"),
        ("collectionId" = Uuid,   Path,   description = "Collection ID"),
        ("itemId"       = Uuid,   Path,   description = "Item ID"),
        ("If-Match"     = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Item removed"),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_item(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current = find_item_scoped(&state, org_id, collection_id, item_id, false).await?;
    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if !collection_item::remove(
        &state.db,
        collection_id,
        item_id,
        Some(listing::version_of(current.updated_at)),
    )
    .await
    .map_err(item_error_to_app_error)?
    {
        return Err(listing::stale_or_gone(find_item_scoped(
            &state,
            org_id,
            collection_id,
            item_id,
            false,
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
        "collection_item.soft_delete",
        "collection_item",
        Some(item_id),
        serde_json::json!({ "collectionId": collection_id, "label": current.label }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Optional request body for restoring a removed item.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UndeleteItemRequest {
    /// Standing collections only: the label to restore the piece under when
    /// its old one is taken, under the same rule as adding.
    label: Option<String>,
}

/// Restore a removed collection item (owner/archivist/conductor).
///
/// A program's piece returns to its old position (the end, if the program
/// has shrunk). A standing collection's piece returns under its old label
/// if that is free; otherwise send a `label`, or get a 409 saying one is
/// needed. The body is optional.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/undelete",
    tag = "collections",
    summary = "Undelete a collection item",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
        ("itemId"       = Uuid, Path, description = "Item ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body(content = Option<UndeleteItemRequest>, description = "Optional; only for a standing collection's taken label"),
    responses(
        (status = 204, description = "Item restored"),
        CommonErrors,
        JsonBodyErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Conflict409,
    )
)]
async fn undelete_item(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    OptionalApiJson(body): OptionalApiJson<UndeleteItemRequest>,
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_item_scoped(&state, org_id, collection_id, item_id, true).await?;
    let label = body.and_then(|b| b.label);

    let restored = collection_item::restore(&state.db, collection_id, item_id, label.as_deref())
        .await
        .map_err(item_error_to_app_error)?;
    if !restored {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection_item.undelete",
        "collection_item",
        Some(item_id),
        serde_json::json!({ "collectionId": collection_id, "label": label }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Request body for swapping two pieces.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SwapItemRequest {
    /// The other piece, in the same collection.
    with_item_id: Uuid,
}

/// Swap two pieces (owner/archivist/conductor): their positions in a
/// program, their labels in a standing collection.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/items/{itemId}/swap",
    tag = "collections",
    summary = "Swap two collection items",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
        ("itemId"       = Uuid, Path, description = "Item ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = SwapItemRequest,
    responses(
        (status = 204, description = "Items swapped"),
        CommonErrors,
        JsonBodyErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn swap_item(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    ApiJson(body): ApiJson<SwapItemRequest>,
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, collection_id).await?;

    if !collection_item::swap(&state.db, collection_id, item_id, body.with_item_id)
        .await
        .map_err(item_error_to_app_error)?
    {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection_item.swap",
        "collection_item",
        Some(item_id),
        serde_json::json!({ "collectionId": collection_id, "withItemId": body.with_item_id }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Preview a renumber of a standing collection (owner/archivist/conductor):
/// every piece's label now and after (1..n in the current order, letters
/// dropped), every section's start now and after (its first piece's new
/// number; a section without pieces keeps one number right after the pieces
/// before it), and the token that applies exactly this.
///
/// A program is always numbered 1..n, so it answers 409.
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/renumber",
    tag = "collections",
    summary = "Preview a renumber",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "What a renumber would change",
            body = collection_item::RenumberPreview),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Conflict409,
    )
)]
async fn preview_renumber(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<collection_item::RenumberPreview>, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, collection_id).await?;
    let preview = collection_item::renumber_preview(&state.db, collection_id)
        .await
        .map_err(item_error_to_app_error)?;
    Ok(Json(preview))
}

/// Request body for applying a renumber.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenumberCollectionRequest {
    /// `previewToken` from `GET …/renumber`.
    preview_token: String,
}

/// The changes a renumber made.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct RenumberResult {
    /// Every piece, in order, with its label before and after.
    changes: Vec<collection_item::RenumberChange>,
    /// Every section, in order, with its start before and after.
    section_changes: Vec<collection_item::SectionRenumberChange>,
}

/// Renumber a standing collection (owner/archivist/conductor): labels 1..n
/// in the current order, closing every gap and dropping letters. Each
/// section then starts at its first piece's new number; a section without
/// pieces keeps one number, right after the pieces before it.
///
/// This changes the numbers printed in the players' folders, so it applies
/// only the preview the client showed: a `previewToken` from a collection
/// that has changed since is a 409, and the client previews again.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{collectionId}/renumber",
    tag = "collections",
    summary = "Renumber a standing collection",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = RenumberCollectionRequest,
    responses(
        (status = 200, description = "Renumbered", body = RenumberResult),
        CommonErrors,
        JsonBodyErrors,
        Validation400,
        Forbidden403,
        NotFound404,
        Conflict409,
    )
)]
async fn renumber_collection(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<RenumberCollectionRequest>,
) -> Result<Json<RenumberResult>, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, collection_id).await?;
    let outcome = collection_item::renumber(&state.db, collection_id, &body.preview_token)
        .await
        .map_err(item_error_to_app_error)?;
    let collection_item::RenumberOutcome {
        changes,
        section_changes,
    } = outcome;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "collection.renumber",
        "collection",
        Some(collection_id),
        serde_json::json!({ "changes": changes, "sectionChanges": section_changes }),
    )
    .await;

    Ok(Json(RenumberResult {
        changes,
        section_changes,
    }))
}

/// Coverage for a collection: which required voices are assigned, and which are
/// rehearsed (UC-13).
///
/// Staff see every voice of every piece. A `musician` with `is_principal = true`
/// sees only their own section — the voices whose instrument is in their
/// `principal_instrument_ids` — which is the one non-staff-facing view in this
/// tree. Everyone else gets `403`.
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{collectionId}/coverage",
    tag = "collections",
    summary = "Coverage for a collection",
    params(
        ("orgId"        = Uuid, Path, description = "Organization ID"),
        ("collectionId" = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Coverage report; scoped to the caller's \
                                      section when they are a principal",
            body = coverage::CoverageReport),
        CommonErrors,
        Validation400,
        Forbidden403,
        NotFound404,
    )
)]
async fn collection_coverage(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<coverage::CoverageReport>, AppError> {
    let scope = require_coverage_viewer_v1(&state, &auth, org_id).await?;
    // Scope the collection to the org before reporting on it: a coverage report
    // names pieces and the people playing them, so a cross-org id must 404
    // rather than leak another orchestra's programme.
    find_collection_scoped(&state, org_id, collection_id).await?;

    let required = match scope {
        CoverageScope::FullProgram => coverage::Required::EveryVoice,
        CoverageScope::Section(instrument_ids) => coverage::Required::Instruments(instrument_ids),
    };
    let report = coverage::for_collection(&state.db, collection_id, &required).await?;
    Ok(Json(report))
}
