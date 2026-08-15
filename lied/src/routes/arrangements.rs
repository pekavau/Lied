//! `/v1/works`, `/v1/orgs/{orgId}/arrangements`,
//! `/v1/orgs/{orgId}/arrangements/{id}/voices`,
//! `/v1/orgs/{orgId}/arrangements/{id}/tags`, `/v1/orgs/{orgId}/tags` —
//! issue #6 (Arrangement metadata management: Works, Arrangements, Voices,
//! Tags — the catalog backbone).

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::require_org_role_v1;
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::{self, Role};
use crate::domain::{arrangement, tag, voice, work};
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
        .routes(routes!(list_works, create_work))
        .routes(routes!(get_work, update_work, delete_work))
        .routes(routes!(list_arrangements, create_arrangement))
        .routes(routes!(
            get_arrangement,
            update_arrangement,
            delete_arrangement
        ))
        .routes(routes!(undelete_arrangement))
        .routes(routes!(list_voices, create_voice))
        .routes(routes!(get_voice, update_voice, delete_voice))
        .routes(routes!(undelete_voice))
        .routes(routes!(list_arrangement_tags, attach_tag))
        .routes(routes!(detach_tag))
        .routes(routes!(list_tags, create_tag))
        .routes(routes!(get_tag, update_tag, delete_tag))
        .routes(routes!(undelete_tag))
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    sort: Option<String>,
    q: Option<String>,
}

fn validation_error(report: garde::Report) -> AppError {
    AppError::Validation(report)
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
    let etag = listing::etag_for(updated_at);
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        axum::http::header::ETAG,
        axum::http::HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("\"0\"")),
    );
    response
}

// ---------------------------------------------------------------------------
// Work — instance-wide.
// ---------------------------------------------------------------------------

/// Request/response body for works.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateWorkRequest {
    title: String,
    composer: Option<String>,
}

/// Request body for `PATCH /v1/works/{id}`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateWorkRequest {
    title: String,
    composer: Option<String>,
}

/// List abstract musical works (any authenticated identity).
#[utoipa::path(
    get,
    path = "/works",
    tag = "works",
    summary = "List works",
    params(
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort field and direction. Allowed: `title` (default asc), `created_at`."),
        ("filter[title]"    = Option<String>, Query, description = "ILIKE filter on title"),
        ("filter[composer]" = Option<String>, Query, description = "ILIKE filter on composer"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated work list",
            body = inline(Page<work::Work>)),
        CommonErrors,
        Validation400,
    )
)]
async fn list_works(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<work::Work>>, AppError> {
    let _ = &auth;
    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        work::SORT_ALLOWLIST,
        ("title", SortDirection::Asc),
    )
    .map_err(validation_error)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters =
        listing::resolve_filters(&raw_filters, work::FILTER_ALLOWLIST).map_err(validation_error)?;
    let title_filter = filters
        .iter()
        .find(|(field, _)| field == "title")
        .map(|(_, value)| value.as_str());
    let composer_filter = filters
        .iter()
        .find(|(field, _)| field == "composer")
        .map(|(_, value)| value.as_str());

    let (items, total) = work::list(
        &state.db,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        title_filter,
        composer_filter,
    )
    .await?;

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Create an abstract work (any authenticated user with at least one membership).
#[utoipa::path(
    post,
    path = "/works",
    tag = "works",
    summary = "Create a work",
    security(("bearer" = []), ("session" = [])),
    request_body = CreateWorkRequest,
    responses(
        (status = 201, description = "Work created", body = work::Work,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        Validation400,
    )
)]
async fn create_work(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Json(body): Json<CreateWorkRequest>,
) -> Result<(StatusCode, Json<work::Work>), AppError> {
    if body.title.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("title"),
            garde::Error::new("title must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    if !auth.user.is_system_admin
        && !membership::has_any_membership(&state.db, auth.user.id).await?
    {
        return Err(AppError::Forbidden);
    }

    let id = Uuid::now_v7();
    let created = work::create(
        &state.db,
        id,
        &body.title,
        body.composer.as_deref(),
        Some(auth.user.id),
    )
    .await?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: None,
            request_id: Some(request_id),
        },
        "work.create",
        "work",
        Some(created.id),
        serde_json::json!({ "title": created.title, "composer": created.composer }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// Fetch one work by ID.
#[utoipa::path(
    get,
    path = "/works/{id}",
    tag = "works",
    summary = "Get a work",
    params(
        ("id" = Uuid, Path, description = "Work ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Work", body = work::Work,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        NotFound404,
    )
)]
async fn get_work(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, AppError> {
    let _ = &auth;
    let found = work::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(etag_response(found.clone(), found.updated_at))
}

/// Update a work's title or composer (creator or system-admin only; requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/works/{id}",
    tag = "works",
    summary = "Update a work",
    params(
        ("id"       = Uuid,   Path,   description = "Work ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateWorkRequest,
    responses(
        (status = 200, description = "Updated work", body = work::Work,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Precondition412,
    )
)]
async fn update_work(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<UpdateWorkRequest>,
) -> Result<Json<work::Work>, AppError> {
    let current = work::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    if !current.is_editable_by(auth.user.id, auth.user.is_system_admin) {
        return Err(AppError::Forbidden);
    }

    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if body.title.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("title"),
            garde::Error::new("title must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    let updated = work::update(&state.db, id, &body.title, body.composer.as_deref())
        .await?
        .ok_or(AppError::NotFound)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: None,
            request_id: Some(request_id),
        },
        "work.update",
        "work",
        Some(id),
        serde_json::json!({
            "before": { "title": current.title, "composer": current.composer },
            "after": { "title": updated.title, "composer": updated.composer },
        }),
    )
    .await;

    Ok(Json(updated))
}

/// Hard-delete a work (creator or system-admin; requires `If-Match`).
///
/// Fails with `409` if the work is still referenced by a live arrangement.
#[utoipa::path(
    delete,
    path = "/works/{id}",
    tag = "works",
    summary = "Delete a work",
    params(
        ("id"       = Uuid,   Path,   description = "Work ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Work deleted"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Conflict409,
        Precondition412,
    )
)]
async fn delete_work(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    let current = work::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    if !current.is_editable_by(auth.user.id, auth.user.is_system_admin) {
        return Err(AppError::Forbidden);
    }

    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: None,
            request_id: Some(request_id),
        },
        "work.delete",
        "work",
        Some(id),
        serde_json::json!({ "title": current.title, "composer": current.composer }),
    )
    .await;

    let deleted = work::delete(&state.db, id).await.map_err(|err| {
        if let sqlx::Error::Database(ref db_err) = err {
            if db_err.is_foreign_key_violation() {
                return AppError::Conflict(
                    "this work is still referenced by an arrangement; remove or reassign it first"
                        .to_string(),
                );
            }
        }
        AppError::Database(err)
    })?;
    if !deleted {
        return Err(AppError::NotFound);
    }

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Arrangement — org-scoped.
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArrangementResponse {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub title: String,
    pub slug: String,
    pub work_id: Option<Uuid>,
    pub instrumentation: Option<String>,
    pub arranger: Option<String>,
    pub publisher: Option<String>,
    pub purchase_date: Option<chrono::NaiveDate>,
    pub license_notes: Option<String>,
    pub copy_count_allowed: Option<i32>,
    pub status: String,
    pub duration_seconds: Option<i32>,
    pub difficulty: Option<i16>,
    pub difficulty_ratings: Option<serde_json::Value>,
    pub difficulty_notes: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<arrangement::Arrangement> for ArrangementResponse {
    fn from(a: arrangement::Arrangement) -> Self {
        Self {
            id: a.id,
            organization_id: a.organization_id,
            title: a.title,
            slug: a.slug,
            work_id: a.work_id,
            instrumentation: a.instrumentation,
            arranger: a.arranger,
            publisher: a.publisher,
            purchase_date: a.purchase_date,
            license_notes: a.license_notes,
            copy_count_allowed: a.copy_count_allowed,
            status: a.status,
            duration_seconds: a.duration_seconds,
            difficulty: a.difficulty,
            difficulty_ratings: a.difficulty_ratings,
            difficulty_notes: a.difficulty_notes,
            created_at: a.created_at,
            updated_at: a.updated_at,
            deleted_at: a.deleted_at,
        }
    }
}

/// Request body for creating or updating an arrangement.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct ArrangementRequest {
    title: String,
    work_id: Option<Uuid>,
    instrumentation: Option<String>,
    arranger: Option<String>,
    publisher: Option<String>,
    purchase_date: Option<chrono::NaiveDate>,
    license_notes: Option<String>,
    copy_count_allowed: Option<i32>,
    status: Option<String>,
    duration_seconds: Option<i32>,
    difficulty: Option<i16>,
    difficulty_ratings: Option<serde_json::Value>,
    difficulty_notes: Option<String>,
}

fn arrangement_error_to_app_error(err: arrangement::ArrangementError) -> AppError {
    match err {
        arrangement::ArrangementError::Database(e) => AppError::Database(e),
        arrangement::ArrangementError::DuplicateSlug => AppError::Conflict(
            "an arrangement with a slug derived from this title already exists in this organization"
                .to_string(),
        ),
        arrangement::ArrangementError::UnknownWork => {
            let mut report = garde::Report::new();
            report.append(
                garde::Path::new("workId"),
                garde::Error::new("workId does not reference a live work"),
            );
            AppError::Validation(report)
        }
    }
}

/// A single-field `garde::Report`, for the filter parse errors below.
fn field_report(field: &str, message: &str) -> garde::Report {
    let mut report = garde::Report::new();
    report.append(
        garde::Path::new(field),
        garde::Error::new(message.to_string()),
    );
    report
}

/// Turn allowlisted `filter[...]` pairs into an [`arrangement::ArrangementSearch`].
///
/// A value that cannot be parsed is a **400**, never a silently dropped facet:
/// a search that quietly ignores `difficultyMin=easy` and returns everything is
/// worse than one that says what it did not understand (the same class of bug
/// the voice-filter fix in #6 addressed).
fn build_search<'a>(
    filters: &'a [(String, String)],
    q: Option<&'a str>,
) -> Result<arrangement::ArrangementSearch<'a>, AppError> {
    fn value<'a>(filters: &'a [(String, String)], field: &str) -> Option<&'a str> {
        filters
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
    }

    fn parse<T: std::str::FromStr>(
        filters: &[(String, String)],
        field: &str,
        expected: &str,
    ) -> Result<Option<T>, AppError> {
        match value(filters, field) {
            None => Ok(None),
            Some(raw) => raw.parse::<T>().map(Some).map_err(|_| {
                validation_error(field_report(
                    &format!("filter[{field}]"),
                    &format!("expected {expected}, got '{raw}'"),
                ))
            }),
        }
    }

    let tag_ids = match value(filters, "tag") {
        None => Vec::new(),
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| {
                Uuid::parse_str(part).map_err(|_| {
                    validation_error(field_report(
                        "filter[tag]",
                        &format!("expected a comma-separated list of tag ids, got '{part}'"),
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    };

    Ok(arrangement::ArrangementSearch {
        q: q.filter(|value| !value.is_empty()),
        status: value(filters, "status"),
        difficulty_min: parse(filters, "difficultyMin", "a whole number")?,
        difficulty_max: parse(filters, "difficultyMax", "a whole number")?,
        duration_min_seconds: parse(filters, "durationMinSeconds", "a whole number of seconds")?,
        duration_max_seconds: parse(filters, "durationMaxSeconds", "a whole number of seconds")?,
        tag_ids,
        instrument_id: parse(filters, "instrumentId", "a tag id")?,
    })
}

/// List arrangements in an organization (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements",
    tag = "arrangements",
    summary = "List arrangements",
    params(
        ("orgId"  = Uuid, Path, description = "Organization ID"),
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort field and direction. Allowed: `title` (default asc), `created_at`, `status`."),
        ("filter[status]" = Option<String>, Query,
            description = "Filter by status: `active` or `archived`"),
        ("q" = Option<String>, Query,
            description = "ILIKE search across title and composer"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated arrangement list",
            body = inline(Page<ArrangementResponse>)),
        CommonErrors,
        Forbidden403,
        Validation400,
    )
)]
async fn list_arrangements(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path(org_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<ArrangementResponse>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;

    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = arrangement::resolve_search_sort(
        q.sort.as_deref(),
        q.q.as_deref().is_some_and(|value| !value.is_empty()),
        ("a.title", SortDirection::Asc),
    )
    .map_err(validation_error)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters = listing::resolve_filters(&raw_filters, arrangement::FILTER_ALLOWLIST)
        .map_err(validation_error)?;
    let search = build_search(&filters, q.q.as_deref())?;

    let (items, total) = arrangement::list_for_org(
        &state.db,
        org_id,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        &search,
    )
    .await?;

    Ok(Json(Page {
        items: items.into_iter().map(ArrangementResponse::from).collect(),
        total,
        limit,
        offset,
    }))
}

/// Add an arrangement to an organization's archive (owner/archivist only).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/arrangements",
    tag = "arrangements",
    summary = "Create an arrangement",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = ArrangementRequest,
    responses(
        (status = 201, description = "Arrangement created", body = ArrangementResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        Validation400,
        Conflict409,
        NotFound404,
    )
)]
async fn create_arrangement(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(org_id): Path<Uuid>,
    Json(body): Json<ArrangementRequest>,
) -> Result<(StatusCode, Json<ArrangementResponse>), AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    if body.title.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("title"),
            garde::Error::new("title must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    let id = Uuid::now_v7();
    let slug = arrangement::slugify(&body.title);
    let status = body.status.as_deref().unwrap_or("active");

    let created = arrangement::create(
        &state.db,
        id,
        org_id,
        &slug,
        arrangement::ArrangementFields {
            title: &body.title,
            work_id: body.work_id,
            instrumentation: body.instrumentation.as_deref(),
            arranger: body.arranger.as_deref(),
            publisher: body.publisher.as_deref(),
            purchase_date: body.purchase_date,
            license_notes: body.license_notes.as_deref(),
            copy_count_allowed: body.copy_count_allowed,
            status,
            duration_seconds: body.duration_seconds,
            difficulty: body.difficulty,
            difficulty_ratings: body.difficulty_ratings.clone(),
            difficulty_notes: body.difficulty_notes.as_deref(),
        },
        Some(auth.user.id),
    )
    .await
    .map_err(arrangement_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "arrangement.create",
        "arrangement",
        Some(created.id),
        serde_json::json!({ "title": created.title, "slug": created.slug }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created.into())))
}

async fn find_arrangement_scoped(
    state: &AppState,
    org_id: Uuid,
    id: Uuid,
) -> Result<arrangement::Arrangement, AppError> {
    let found = arrangement::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.organization_id != org_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

async fn find_arrangement_scoped_including_deleted(
    state: &AppState,
    org_id: Uuid,
    id: Uuid,
) -> Result<arrangement::Arrangement, AppError> {
    let found = arrangement::find_by_id_including_deleted(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.organization_id != org_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

/// Fetch one arrangement by ID (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{id}",
    tag = "arrangements",
    summary = "Get an arrangement",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Arrangement ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Arrangement", body = ArrangementResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_arrangement(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    let found = find_arrangement_scoped(&state, org_id, id).await?;
    let updated_at = found.updated_at;
    Ok(etag_response(ArrangementResponse::from(found), updated_at))
}

/// Update arrangement metadata (owner/archivist; requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/arrangements/{id}",
    tag = "arrangements",
    summary = "Update an arrangement",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("id"       = Uuid,   Path,   description = "Arrangement ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = ArrangementRequest,
    responses(
        (status = 200, description = "Updated arrangement", body = ArrangementResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Conflict409,
        Precondition412,
    )
)]
async fn update_arrangement(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(body): Json<ArrangementRequest>,
) -> Result<Json<ArrangementResponse>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    let current = find_arrangement_scoped(&state, org_id, id).await?;

    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if body.title.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("title"),
            garde::Error::new("title must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    let status = body.status.as_deref().unwrap_or(&current.status);

    let updated = arrangement::update(
        &state.db,
        id,
        arrangement::ArrangementFields {
            title: &body.title,
            work_id: body.work_id,
            instrumentation: body.instrumentation.as_deref(),
            arranger: body.arranger.as_deref(),
            publisher: body.publisher.as_deref(),
            purchase_date: body.purchase_date,
            license_notes: body.license_notes.as_deref(),
            copy_count_allowed: body.copy_count_allowed,
            status,
            duration_seconds: body.duration_seconds,
            difficulty: body.difficulty,
            difficulty_ratings: body.difficulty_ratings.clone(),
            difficulty_notes: body.difficulty_notes.as_deref(),
        },
    )
    .await
    .map_err(arrangement_error_to_app_error)?
    .ok_or(AppError::NotFound)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "arrangement.update",
        "arrangement",
        Some(id),
        serde_json::json!({
            "before": { "title": current.title, "status": current.status },
            "after": { "title": updated.title, "status": updated.status },
        }),
    )
    .await;

    Ok(Json(updated.into()))
}

/// Soft-delete an arrangement (owner/archivist; requires `If-Match`).
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/arrangements/{id}",
    tag = "arrangements",
    summary = "Soft-delete an arrangement",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("id"       = Uuid,   Path,   description = "Arrangement ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Arrangement soft-deleted"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_arrangement(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    let current = find_arrangement_scoped(&state, org_id, id).await?;

    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let deleted = arrangement::soft_delete(&state.db, id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "arrangement.soft_delete",
        "arrangement",
        Some(id),
        serde_json::json!({ "title": current.title, "slug": current.slug }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Restore a soft-deleted arrangement (owner/archivist only).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/arrangements/{id}/undelete",
    tag = "arrangements",
    summary = "Undelete an arrangement",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Arrangement ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Arrangement restored"),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn undelete_arrangement(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    let current = find_arrangement_scoped_including_deleted(&state, org_id, id).await?;

    let restored = arrangement::undelete(&state.db, id).await?;
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
        "arrangement.undelete",
        "arrangement",
        Some(id),
        serde_json::json!({ "title": current.title, "slug": current.slug }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Voice — under an Arrangement.
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VoiceResponse {
    pub id: Uuid,
    pub arrangement_id: Uuid,
    pub name: String,
    pub slug: String,
    pub instrument_id: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<voice::Voice> for VoiceResponse {
    fn from(v: voice::Voice) -> Self {
        Self {
            id: v.id,
            arrangement_id: v.arrangement_id,
            name: v.name,
            slug: v.slug,
            instrument_id: v.instrument_id,
            created_at: v.created_at,
            updated_at: v.updated_at,
            deleted_at: v.deleted_at,
        }
    }
}

/// Request body for creating or updating a voice.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct VoiceRequest {
    name: String,
    instrument_id: Uuid,
}

fn voice_error_to_app_error(err: voice::VoiceError) -> AppError {
    match err {
        voice::VoiceError::Database(e) => AppError::Database(e),
        voice::VoiceError::DuplicateSlug => AppError::Conflict(
            "a voice with a slug derived from this name already exists in this arrangement"
                .to_string(),
        ),
        voice::VoiceError::UnknownInstrument => {
            let mut report = garde::Report::new();
            report.append(
                garde::Path::new("instrumentId"),
                garde::Error::new("instrumentId does not reference a live instrument"),
            );
            AppError::Validation(report)
        }
    }
}

#[derive(Debug, Deserialize)]
struct VoiceListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    sort: Option<String>,
}

/// List voices in an arrangement (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{id}/voices",
    tag = "voices",
    summary = "List voices",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Arrangement ID"),
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort field and direction. Allowed: `name` (default asc), `created_at`."),
        ("filter[instrumentId]" = Option<String>, Query,
            description = "Filter by instrument UUID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated voice list",
            body = inline(Page<VoiceResponse>)),
        CommonErrors,
        Forbidden403,
        Validation400,
        NotFound404,
    )
)]
async fn list_voices(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arrangement_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<VoiceListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<VoiceResponse>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;

    find_arrangement_scoped(&state, org_id, arrangement_id).await?;

    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        voice::SORT_ALLOWLIST,
        ("v.name", SortDirection::Asc),
    )
    .map_err(validation_error)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters = listing::resolve_filters(&raw_filters, voice::FILTER_ALLOWLIST)
        .map_err(validation_error)?;
    let instrument_id_filter = match filters.iter().find(|(field, _)| field == "instrumentId") {
        Some((_, value)) => Some(Uuid::parse_str(value).map_err(|_| {
            let mut report = garde::Report::new();
            report.append(
                garde::Path::new("filter[instrumentId]"),
                garde::Error::new("instrumentId must be a valid UUID"),
            );
            AppError::Validation(report)
        })?),
        None => None,
    };

    let (items, total) = voice::list_for_arrangement(
        &state.db,
        arrangement_id,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        instrument_id_filter,
    )
    .await?;

    Ok(Json(Page {
        items: items.into_iter().map(VoiceResponse::from).collect(),
        total,
        limit,
        offset,
    }))
}

/// Add a voice (instrument part) to an arrangement (owner/archivist only).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/arrangements/{id}/voices",
    tag = "voices",
    summary = "Create a voice",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Arrangement ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = VoiceRequest,
    responses(
        (status = 201, description = "Voice created", body = VoiceResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        Validation400,
        Conflict409,
        NotFound404,
    )
)]
async fn create_voice(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arrangement_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<VoiceRequest>,
) -> Result<(StatusCode, Json<VoiceResponse>), AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    find_arrangement_scoped(&state, org_id, arrangement_id).await?;

    if body.name.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("name"),
            garde::Error::new("name must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    let id = Uuid::now_v7();
    let slug = voice::slugify(&body.name);

    let created = voice::create(
        &state.db,
        id,
        arrangement_id,
        &body.name,
        &slug,
        body.instrument_id,
        Some(auth.user.id),
    )
    .await
    .map_err(voice_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "voice.create",
        "voice",
        Some(created.id),
        serde_json::json!({ "name": created.name, "slug": created.slug, "arrangementId": arrangement_id }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created.into())))
}

async fn find_voice_scoped(
    state: &AppState,
    org_id: Uuid,
    arrangement_id: Uuid,
    id: Uuid,
) -> Result<voice::Voice, AppError> {
    find_arrangement_scoped(state, org_id, arrangement_id).await?;
    let found = voice::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.arrangement_id != arrangement_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

async fn find_voice_scoped_including_deleted(
    state: &AppState,
    org_id: Uuid,
    arrangement_id: Uuid,
    id: Uuid,
) -> Result<voice::Voice, AppError> {
    find_arrangement_scoped_including_deleted(state, org_id, arrangement_id).await?;
    let found = voice::find_by_id_including_deleted(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.arrangement_id != arrangement_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

/// Fetch one voice by ID (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}",
    tag = "voices",
    summary = "Get a voice",
    params(
        ("orgId"         = Uuid, Path, description = "Organization ID"),
        ("arrangementId" = Uuid, Path, description = "Arrangement ID"),
        ("id"            = Uuid, Path, description = "Voice ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Voice", body = VoiceResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_voice(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arrangement_id, id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    let found = find_voice_scoped(&state, org_id, arrangement_id, id).await?;
    let updated_at = found.updated_at;
    Ok(etag_response(VoiceResponse::from(found), updated_at))
}

/// Update a voice's name or instrument (owner/archivist; requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}",
    tag = "voices",
    summary = "Update a voice",
    params(
        ("orgId"         = Uuid,   Path,   description = "Organization ID"),
        ("arrangementId" = Uuid,   Path,   description = "Arrangement ID"),
        ("id"            = Uuid,   Path,   description = "Voice ID"),
        ("If-Match"      = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = VoiceRequest,
    responses(
        (status = 200, description = "Updated voice", body = VoiceResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Conflict409,
        Precondition412,
    )
)]
async fn update_voice(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arrangement_id, id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    Json(body): Json<VoiceRequest>,
) -> Result<Json<VoiceResponse>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    let current = find_voice_scoped(&state, org_id, arrangement_id, id).await?;

    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if body.name.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("name"),
            garde::Error::new("name must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    let updated = voice::update(&state.db, id, &body.name, body.instrument_id)
        .await
        .map_err(voice_error_to_app_error)?
        .ok_or(AppError::NotFound)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "voice.update",
        "voice",
        Some(id),
        serde_json::json!({
            "before": { "name": current.name, "instrumentId": current.instrument_id },
            "after": { "name": updated.name, "instrumentId": updated.instrument_id },
        }),
    )
    .await;

    Ok(Json(updated.into()))
}

/// Soft-delete a voice (owner/archivist; requires `If-Match`).
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}",
    tag = "voices",
    summary = "Soft-delete a voice",
    params(
        ("orgId"         = Uuid,   Path,   description = "Organization ID"),
        ("arrangementId" = Uuid,   Path,   description = "Arrangement ID"),
        ("id"            = Uuid,   Path,   description = "Voice ID"),
        ("If-Match"      = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Voice soft-deleted"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_voice(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arrangement_id, id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    let current = find_voice_scoped(&state, org_id, arrangement_id, id).await?;

    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let deleted = voice::soft_delete(&state.db, id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "voice.soft_delete",
        "voice",
        Some(id),
        serde_json::json!({ "name": current.name, "slug": current.slug }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Restore a soft-deleted voice (owner/archivist only).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}/undelete",
    tag = "voices",
    summary = "Undelete a voice",
    params(
        ("orgId"         = Uuid, Path, description = "Organization ID"),
        ("arrangementId" = Uuid, Path, description = "Arrangement ID"),
        ("id"            = Uuid, Path, description = "Voice ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Voice restored"),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn undelete_voice(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arrangement_id, id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    let current = find_voice_scoped_including_deleted(&state, org_id, arrangement_id, id).await?;

    let restored = voice::undelete(&state.db, id).await?;
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
        "voice.undelete",
        "voice",
        Some(id),
        serde_json::json!({ "name": current.name, "slug": current.slug }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Tag + ArrangementTag — owner/archivist gated.
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TagResponse {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub kind: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<tag::Tag> for TagResponse {
    fn from(t: tag::Tag) -> Self {
        Self {
            id: t.id,
            organization_id: t.organization_id,
            name: t.name,
            kind: t.kind,
            created_at: t.created_at,
            updated_at: t.updated_at,
            deleted_at: t.deleted_at,
        }
    }
}

/// Request body for creating or updating a tag.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct TagRequest {
    name: String,
    kind: Option<String>,
}

fn tag_error_to_app_error(err: tag::TagError) -> AppError {
    match err {
        tag::TagError::Database(e) => AppError::Database(e),
        tag::TagError::Duplicate => AppError::Conflict(
            "a tag with this name and kind already exists in this organization".to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct TagListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    sort: Option<String>,
}

/// List tags in an organization (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/tags",
    tag = "tags",
    summary = "List tags",
    params(
        ("orgId"  = Uuid, Path, description = "Organization ID"),
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort field and direction. Allowed: `name` (default asc), `kind`, `created_at`."),
        ("filter[kind]" = Option<String>, Query,
            description = "Filter by tag kind, e.g. `theme`, `mood`, `era`"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated tag list",
            body = inline(Page<TagResponse>)),
        CommonErrors,
        Forbidden403,
        Validation400,
    )
)]
async fn list_tags(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path(org_id): Path<Uuid>,
    Query(q): Query<TagListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<TagResponse>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;

    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        tag::SORT_ALLOWLIST,
        ("name", SortDirection::Asc),
    )
    .map_err(validation_error)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters =
        listing::resolve_filters(&raw_filters, tag::FILTER_ALLOWLIST).map_err(validation_error)?;
    let kind_filter = filters
        .iter()
        .find(|(field, _)| field == "kind")
        .map(|(_, value)| value.as_str());

    let (items, total) = tag::list_for_org(
        &state.db,
        org_id,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        kind_filter,
    )
    .await?;

    Ok(Json(Page {
        items: items.into_iter().map(TagResponse::from).collect(),
        total,
        limit,
        offset,
    }))
}

/// Create a tag in an organization (owner/archivist only).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/tags",
    tag = "tags",
    summary = "Create a tag",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = TagRequest,
    responses(
        (status = 201, description = "Tag created", body = TagResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        Validation400,
        Conflict409,
    )
)]
async fn create_tag(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(org_id): Path<Uuid>,
    Json(body): Json<TagRequest>,
) -> Result<(StatusCode, Json<TagResponse>), AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    if body.name.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("name"),
            garde::Error::new("name must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    let id = Uuid::now_v7();
    let created = tag::create(
        &state.db,
        id,
        org_id,
        &body.name,
        body.kind.as_deref(),
        Some(auth.user.id),
    )
    .await
    .map_err(tag_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "tag.create",
        "tag",
        Some(created.id),
        serde_json::json!({ "name": created.name, "kind": created.kind }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created.into())))
}

async fn find_tag_scoped(state: &AppState, org_id: Uuid, id: Uuid) -> Result<tag::Tag, AppError> {
    let found = tag::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.organization_id != org_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

/// Fetch one tag by ID (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/tags/{id}",
    tag = "tags",
    summary = "Get a tag",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Tag ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Tag", body = TagResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_tag(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    let found = find_tag_scoped(&state, org_id, id).await?;
    let updated_at = found.updated_at;
    Ok(etag_response(TagResponse::from(found), updated_at))
}

/// Update a tag's name or kind (owner/archivist; requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/tags/{id}",
    tag = "tags",
    summary = "Update a tag",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("id"       = Uuid,   Path,   description = "Tag ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = TagRequest,
    responses(
        (status = 200, description = "Updated tag", body = TagResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Conflict409,
        Precondition412,
    )
)]
async fn update_tag(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(body): Json<TagRequest>,
) -> Result<Json<TagResponse>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    let current = find_tag_scoped(&state, org_id, id).await?;

    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    if body.name.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("name"),
            garde::Error::new("name must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    let updated = tag::update(&state.db, id, &body.name, body.kind.as_deref())
        .await
        .map_err(tag_error_to_app_error)?
        .ok_or(AppError::NotFound)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "tag.update",
        "tag",
        Some(id),
        serde_json::json!({
            "before": { "name": current.name, "kind": current.kind },
            "after": { "name": updated.name, "kind": updated.kind },
        }),
    )
    .await;

    Ok(Json(updated.into()))
}

/// Soft-delete a tag (owner/archivist; requires `If-Match`).
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/tags/{id}",
    tag = "tags",
    summary = "Soft-delete a tag",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("id"       = Uuid,   Path,   description = "Tag ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Tag soft-deleted"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_tag(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    let current = find_tag_scoped(&state, org_id, id).await?;

    listing::check_if_match(if_match_header(&headers), current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let deleted = tag::soft_delete(&state.db, id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "tag.soft_delete",
        "tag",
        Some(id),
        serde_json::json!({ "name": current.name, "kind": current.kind }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Restore a soft-deleted tag (owner/archivist only).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/tags/{id}/undelete",
    tag = "tags",
    summary = "Undelete a tag",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Tag ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Tag restored"),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn undelete_tag(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    // Fetch the tag (including soft-deleted) to verify org scope and gather
    // audit fields. `tag::find_by_id` filters deleted rows, so we query directly.
    let row = sqlx::query!(
        r#"SELECT organization_id, name, kind FROM tag WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await
    .map_err(AppError::Database)?
    .ok_or(AppError::NotFound)?;
    if row.organization_id != org_id {
        return Err(AppError::NotFound);
    }

    let restored = tag::undelete(&state.db, id).await?;
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
        "tag.undelete",
        "tag",
        Some(id),
        serde_json::json!({ "name": row.name, "kind": row.kind }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// ArrangementTag — tag-to-arrangement attach/detach/list.
// ---------------------------------------------------------------------------

/// List tags attached to a specific arrangement (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{id}/tags",
    tag = "tags",
    summary = "List arrangement tags",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Arrangement ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Tags attached to the arrangement",
            body = Vec<TagResponse>),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn list_arrangement_tags(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<TagResponse>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    find_arrangement_scoped(&state, org_id, id).await?;

    let tags = tag::list_tags_for_arrangement(&state.db, id).await?;
    Ok(Json(tags.into_iter().map(TagResponse::from).collect()))
}

/// Request body for attaching a tag to an arrangement.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct AttachTagRequest {
    tag_id: Uuid,
}

/// Attach an existing tag to an arrangement (owner/archivist only).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/arrangements/{id}/tags",
    tag = "tags",
    summary = "Attach a tag to an arrangement",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Arrangement ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = AttachTagRequest,
    responses(
        (status = 201, description = "Tag attached"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Conflict409,
    )
)]
async fn attach_tag(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    Json(body): Json<AttachTagRequest>,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    find_arrangement_scoped(&state, org_id, id).await?;
    find_tag_scoped(&state, org_id, body.tag_id).await?;

    let join_id = Uuid::now_v7();
    tag::attach(&state.db, join_id, id, body.tag_id)
        .await
        .map_err(|err| match err {
            tag::ArrangementTagError::Database(e) => AppError::Database(e),
            tag::ArrangementTagError::Duplicate => {
                AppError::Conflict("this tag is already attached to this arrangement".to_string())
            }
        })?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "arrangement_tag.create",
        "arrangement_tag",
        Some(join_id),
        serde_json::json!({ "arrangementId": id, "tagId": body.tag_id }),
    )
    .await;

    Ok(StatusCode::CREATED)
}

/// Detach a tag from an arrangement (owner/archivist only).
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/arrangements/{arrangementId}/tags/{tagId}",
    tag = "tags",
    summary = "Detach a tag from an arrangement",
    params(
        ("orgId"         = Uuid, Path, description = "Organization ID"),
        ("arrangementId" = Uuid, Path, description = "Arrangement ID"),
        ("tagId"         = Uuid, Path, description = "Tag ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Tag detached"),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn detach_tag(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arrangement_id, tag_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    find_arrangement_scoped(&state, org_id, arrangement_id).await?;

    let detached = tag::detach(&state.db, arrangement_id, tag_id).await?;
    if !detached {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "arrangement_tag.delete",
        "arrangement_tag",
        None,
        serde_json::json!({ "arrangementId": arrangement_id, "tagId": tag_id }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}
