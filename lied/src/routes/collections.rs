//! `/v1/orgs/{orgId}/collections` and `.../collections/{id}/items` — issue #9
//! (Collections + CollectionItems: program assembly and standing repertoire).
//!
//! Reads require any org membership (`musician`+); writes (build/edit) require
//! `owner`/`archivist`/`conductor` (CLAUDE.md Permission matrix), enforced via
//! [`require_collection_editor_v1`]. Optimistic concurrency (ETag/`If-Match`),
//! RFC 7807 errors, and an `audit()` on every write, per the API guidelines.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::{require_collection_editor_v1, require_org_role_v1};
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::Role;
use crate::domain::{collection, collection_item};
use crate::error::AppError;
use crate::listing::{self, SortDirection};
use crate::pagination::Page;
use crate::routes::openapi::{
    CommonErrors, Conflict409, Forbidden403, NotFound404, Precondition412, Validation400,
};
use crate::routes::RequestId;
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
        .routes(routes!(list_items, add_item))
        .routes(routes!(get_item, update_item, delete_item))
        .routes(routes!(undelete_item))
}

// ── shared helpers ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    sort: Option<String>,
}

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

/// Like [`etag_response`] but with an explicit status (e.g. `201` on create),
/// so the `ETag` response header the OpenAPI spec declares is actually emitted.
fn etag_response_status<T: Serialize>(
    status: StatusCode,
    body: T,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> axum::response::Response {
    let etag = listing::etag_for(updated_at);
    let mut response = Json(body).into_response();
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::ETAG,
        axum::http::HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("\"0\"")),
    );
    response
}

/// Piece numbers are 1-based and bounded well below the reorder parking offset
/// (1,000,000) so index arithmetic can't overflow or collide (review #4).
const MAX_INDEX: i32 = 999_999;

fn validate_index(index: i32) -> Result<(), AppError> {
    if (1..=MAX_INDEX).contains(&index) {
        Ok(())
    } else {
        Err(empty_field("index", "index must be between 1 and 999999"))
    }
}

fn empty_field(field: &str, msg: &str) -> AppError {
    let mut report = garde::Report::new();
    report.append(garde::Path::new(field), garde::Error::new(msg.to_string()));
    AppError::Validation(report)
}

fn collection_error_to_app_error(err: collection::CollectionError) -> AppError {
    match err {
        collection::CollectionError::DuplicateSlug => {
            AppError::Conflict("a collection with this name already exists".into())
        }
        collection::CollectionError::Database(e) => AppError::Database(e),
    }
}

fn item_error_to_app_error(err: collection_item::CollectionItemError) -> AppError {
    match err {
        collection_item::CollectionItemError::DuplicateIndex => {
            AppError::Conflict("an item already exists at this index".into())
        }
        collection_item::CollectionItemError::UnknownReference => {
            AppError::Conflict("the referenced arrangement does not exist".into())
        }
        collection_item::CollectionItemError::InvalidReorder => {
            AppError::Conflict("orderedIds must be exactly the collection's current items".into())
        }
        collection_item::CollectionItemError::Database(e) => AppError::Database(e),
    }
}

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

/// Request body for creating or updating a collection.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct CollectionRequest {
    name: String,
    /// `program` (fixed concert sequence) or `standing` (flexible repertoire).
    #[serde(rename = "type")]
    collection_type: String,
}

/// List an organization's collections (requires org membership).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections",
    tag = "collections",
    summary = "List collections",
    params(
        ("orgId"  = Uuid, Path, description = "Organization ID"),
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort field and direction. Allowed: `name` (default asc), `createdAt`."),
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
    Query(q): Query<ListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<collection::Collection>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;

    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        collection::SORT_ALLOWLIST,
        ("name", SortDirection::Asc),
    )
    .map_err(AppError::Validation)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
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
    request_body = CollectionRequest,
    responses(
        (status = 201, description = "Collection created", body = collection::Collection,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
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
    Json(body): Json<CollectionRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;

    if body.name.trim().is_empty() {
        return Err(empty_field("name", "name must not be empty"));
    }
    if !collection::is_valid_type(&body.collection_type) {
        return Err(empty_field("type", "type must be `program` or `standing`"));
    }

    let id = Uuid::now_v7();
    let slug = collection::slugify(&body.name);
    let created = collection::create(
        &state.db,
        id,
        org_id,
        &body.name,
        &slug,
        &body.collection_type,
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
        serde_json::json!({ "name": created.name, "slug": created.slug, "type": created.collection_type }),
    )
    .await;

    let updated_at = created.updated_at;
    Ok(etag_response_status(
        StatusCode::CREATED,
        created,
        updated_at,
    ))
}

/// Fetch one collection (requires org membership).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{id}",
    tag = "collections",
    summary = "Get a collection",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Collection", body = collection::Collection,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_collection(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    let found = find_collection_scoped(&state, org_id, id).await?;
    let updated_at = found.updated_at;
    Ok(etag_response(found, updated_at))
}

/// Update a collection's name/type (owner/archivist/conductor; requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/collections/{id}",
    tag = "collections",
    summary = "Update a collection",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("id"       = Uuid,   Path,   description = "Collection ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = CollectionRequest,
    responses(
        (status = 200, description = "Updated collection", body = collection::Collection,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
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
    Json(body): Json<CollectionRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;

    let current = find_collection_scoped(&state, org_id, id).await?;
    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if body.name.trim().is_empty() {
        return Err(empty_field("name", "name must not be empty"));
    }
    if !collection::is_valid_type(&body.collection_type) {
        return Err(empty_field("type", "type must be `program` or `standing`"));
    }

    let updated = collection::update(&state.db, id, &body.name, &body.collection_type)
        .await
        .map_err(collection_error_to_app_error)?
        .ok_or(AppError::NotFound)?;

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
            "before": { "name": current.name, "type": current.collection_type },
            "after": { "name": updated.name, "type": updated.collection_type },
        }),
    )
    .await;

    let updated_at = updated.updated_at;
    Ok(etag_response(updated, updated_at))
}

/// Soft-delete a collection (owner/archivist/conductor; requires `If-Match`).
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/collections/{id}",
    tag = "collections",
    summary = "Soft-delete a collection",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("id"       = Uuid,   Path,   description = "Collection ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Collection soft-deleted"),
        CommonErrors,
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

    if !collection::soft_delete(&state.db, id).await? {
        return Err(AppError::NotFound);
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
    path = "/orgs/{orgId}/collections/{id}/undelete",
    tag = "collections",
    summary = "Undelete a collection",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Collection restored"),
        CommonErrors,
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
#[serde(rename_all = "camelCase")]
struct ReorderRequest {
    /// The item IDs in their new order; reassigned indices `1..=n`.
    ordered_ids: Vec<Uuid>,
}

/// Reorder a collection's items (owner/archivist/conductor).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{id}/reorder",
    tag = "collections",
    summary = "Reorder collection items",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = ReorderRequest,
    responses(
        (status = 204, description = "Items reordered"),
        CommonErrors,
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
    Json(body): Json<ReorderRequest>,
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

/// List a collection's items in index order (requires org membership). A
/// soft-deleted arrangement is surfaced with `arrangementRemoved: true` and its
/// slug, not dropped (hide-with-references).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/collections/{id}/items",
    tag = "collections",
    summary = "List collection items",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Items in index order",
            body = Vec<collection_item::CollectionItemView>),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn list_items(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<collection_item::CollectionItemView>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    find_collection_scoped(&state, org_id, id).await?;
    let items = collection_item::list_for_collection(&state.db, id).await?;
    Ok(Json(items))
}

/// Request body for adding an item to a collection.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct AddItemRequest {
    arrangement_id: Uuid,
    index: i32,
}

/// Add an arrangement to a collection at an index (owner/archivist/conductor).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/collections/{id}/items",
    tag = "collections",
    summary = "Add a collection item",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Collection ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = AddItemRequest,
    responses(
        (status = 201, description = "Item added", body = collection_item::CollectionItem,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
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
    Json(body): Json<AddItemRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    find_collection_scoped(&state, org_id, id).await?;
    validate_index(body.index)?;

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
        body.index,
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
        serde_json::json!({ "collectionId": id, "arrangementId": body.arrangement_id, "index": body.index }),
    )
    .await;

    let updated_at = created.updated_at;
    Ok(etag_response_status(
        StatusCode::CREATED,
        created,
        updated_at,
    ))
}

/// Fetch one collection item (requires org membership).
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
        Forbidden403,
        NotFound404,
    )
)]
async fn get_item(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    let item = find_item_scoped(&state, org_id, collection_id, item_id, false).await?;
    let updated_at = item.updated_at;
    Ok(etag_response(item, updated_at))
}

/// Request body for moving an item to a new index.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateItemRequest {
    index: i32,
}

/// Move a collection item to a new index (owner/archivist/conductor; requires `If-Match`).
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
    Json(body): Json<UpdateItemRequest>,
) -> Result<axum::response::Response, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current = find_item_scoped(&state, org_id, collection_id, item_id, false).await?;
    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;
    validate_index(body.index)?;

    let updated = collection_item::update_index(&state.db, item_id, body.index)
        .await
        .map_err(item_error_to_app_error)?
        .ok_or(AppError::NotFound)?;

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
        serde_json::json!({ "before": { "index": current.index }, "after": { "index": updated.index } }),
    )
    .await;

    let updated_at = updated.updated_at;
    Ok(etag_response(updated, updated_at))
}

/// Remove a collection item (owner/archivist/conductor; requires `If-Match`).
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

    if !collection_item::soft_delete(&state.db, item_id).await? {
        return Err(AppError::NotFound);
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
        serde_json::json!({ "collectionId": collection_id, "index": current.index }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Restore a removed collection item (owner/archivist/conductor).
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
    responses(
        (status = 204, description = "Item restored"),
        CommonErrors,
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
) -> Result<StatusCode, AppError> {
    require_collection_editor_v1(&state, &auth, org_id).await?;
    let current = find_item_scoped(&state, org_id, collection_id, item_id, true).await?;

    let restored = collection_item::undelete(&state.db, item_id)
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
        serde_json::json!({ "collectionId": collection_id, "index": current.index }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}
