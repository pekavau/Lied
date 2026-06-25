//! `/v1/works`, `/v1/orgs/{orgId}/arrangements`,
//! `/v1/orgs/{orgId}/arrangements/{id}/voices`,
//! `/v1/orgs/{orgId}/arrangements/{id}/tags`, `/v1/orgs/{orgId}/tags` —
//! issue #6 (Arrangement metadata management: Works, Arrangements, Voices,
//! Tags — the catalog backbone).
//!
//! Follows the conventions established in [`crate::routes::orgs`]:
//! allowlisted sort/filter (`crate::listing`), ETag/`If-Match` optimistic
//! concurrency, RFC 7807 errors via [`AppError`], and an `audit()` call on
//! every write.
//!
//! **Undelete REST shape.** No existing entity in this codebase has a
//! soft-delete + undelete pair yet exposed over `/v1` (Organization/User are
//! hard-delete-only). This module establishes the convention:
//! `POST /v1/.../{id}/undelete` — a dedicated action sub-resource, mirroring
//! how `/v1/app-passwords/{id}` uses `DELETE` for revoke (a state transition,
//! not a representation replacement). `DELETE` soft-deletes (still requires
//! `If-Match`, like every other mutating verb); `POST .../undelete` reverses
//! it. Both are audited.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::authz::require_org_role_v1;
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::{self, Role};
use crate::domain::{arrangement, tag, voice, work};
use crate::error::AppError;
use crate::listing::{self, SortDirection};
use crate::pagination::Page;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/works", get(list_works).post(create_work))
        .route(
            "/works/:id",
            get(get_work).patch(update_work).delete(delete_work),
        )
        .route(
            "/orgs/:org_id/arrangements",
            get(list_arrangements).post(create_arrangement),
        )
        .route(
            "/orgs/:org_id/arrangements/:id",
            get(get_arrangement)
                .patch(update_arrangement)
                .delete(delete_arrangement),
        )
        .route(
            "/orgs/:org_id/arrangements/:id/undelete",
            post(undelete_arrangement),
        )
        .route(
            "/orgs/:org_id/arrangements/:id/voices",
            get(list_voices).post(create_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arrangement_id/voices/:id",
            get(get_voice).patch(update_voice).delete(delete_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arrangement_id/voices/:id/undelete",
            post(undelete_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:id/tags",
            get(list_arrangement_tags).post(attach_tag),
        )
        .route(
            "/orgs/:org_id/arrangements/:arrangement_id/tags/:tag_id",
            axum::routing::delete(detach_tag),
        )
        .route("/orgs/:org_id/tags", get(list_tags).post(create_tag))
        .route(
            "/orgs/:org_id/tags/:id",
            get(get_tag).patch(update_tag).delete(delete_tag),
        )
        .route("/orgs/:org_id/tags/:id/undelete", post(undelete_tag))
}

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
// Work — instance-wide. Create: any authenticated user with >= 1 Membership
// in any org. Edit/delete: creator or system admin (CLAUDE.md Decisions).
// ---------------------------------------------------------------------------

/// `GET /v1/works?limit=&offset=&sort=&filter[title]=&filter[composer]=` —
/// open to any authenticated identity (instance-wide catalog browsing).
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkRequest {
    title: String,
    composer: Option<String>,
}

/// `POST /v1/works` — open to any authenticated user holding at least one
/// `Membership` (in any org). CLAUDE.md Decisions: "creating a Work is open
/// to any authenticated user with at least one Membership in any org."
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

/// `GET /v1/works/{id}` — open to any authenticated identity. Sets `ETag`.
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateWorkRequest {
    title: String,
    composer: Option<String>,
}

/// `PATCH /v1/works/{id}` — creator or system admin only. Requires
/// `If-Match`; stale or missing -> `412`.
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

/// `DELETE /v1/works/{id}` — creator or system admin only. Hard delete (no
/// `deleted_at` on Work). Requires `If-Match`; stale or missing -> `412`. A
/// Work still referenced by a live Arrangement -> `409` (FK violation,
/// mapped cleanly; see [`work::delete`] doc comment).
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
// Arrangement — org-scoped. owner/archivist gated (CLAUDE.md Permission
// matrix: "Upload/edit arrangements & files").
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct ArrangementResponse {
    id: Uuid,
    organization_id: Uuid,
    title: String,
    slug: String,
    work_id: Option<Uuid>,
    instrumentation: Option<String>,
    arranger: Option<String>,
    publisher: Option<String>,
    purchase_date: Option<chrono::NaiveDate>,
    license_notes: Option<String>,
    copy_count_allowed: Option<i32>,
    status: String,
    duration_seconds: Option<i32>,
    difficulty: Option<i16>,
    difficulty_ratings: Option<serde_json::Value>,
    difficulty_notes: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
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

#[derive(Deserialize)]
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

/// `GET /v1/orgs/{orgId}/arrangements?limit=&offset=&sort=&filter[status]=&q=`
/// — requires at least `musician` membership (read access; CLAUDE.md
/// Permission matrix: "Read assigned parts" extends to browsing the org's
/// own catalog). `q` performs the phase-1 ILIKE search across
/// `Arrangement.title` and `Work.composer`.
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

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        arrangement::SORT_ALLOWLIST,
        ("a.title", SortDirection::Asc),
    )
    .map_err(validation_error)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters = listing::resolve_filters(&raw_filters, arrangement::FILTER_ALLOWLIST)
        .map_err(validation_error)?;
    let status_filter = filters
        .iter()
        .find(|(field, _)| field == "status")
        .map(|(_, value)| value.as_str());

    let (items, total) = arrangement::list_for_org(
        &state.db,
        org_id,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        status_filter,
        q.q.as_deref(),
    )
    .await?;

    Ok(Json(Page {
        items: items.into_iter().map(ArrangementResponse::from).collect(),
        total,
        limit,
        offset,
    }))
}

/// `POST /v1/orgs/{orgId}/arrangements` — owner/archivist only.
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

/// Look up an arrangement and verify it belongs to `org_id` (path-scoping
/// consistency, same as `find_member_scoped` in `routes::orgs`).
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

/// Same as [`find_arrangement_scoped`] but also returns soft-deleted rows —
/// used by `undelete`.
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

/// `GET /v1/orgs/{orgId}/arrangements/{id}` — requires at least `musician`.
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

/// `PATCH /v1/orgs/{orgId}/arrangements/{id}` — owner/archivist only. `slug`
/// and `organizationId` are immutable; not accepted in the body. Requires
/// `If-Match`; stale or missing -> `412`.
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

/// `DELETE /v1/orgs/{orgId}/arrangements/{id}` — owner/archivist only.
/// Soft-delete only (`deleted_at` set on this row, not its Voices —
/// CLAUDE.md hide-with-references). Requires `If-Match`; stale or missing
/// -> `412`.
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

/// `POST /v1/orgs/{orgId}/arrangements/{id}/undelete` — owner/archivist
/// only. Clears `deleted_at`.
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
// Voice — under an Arrangement. owner/archivist gated (same matrix row as
// Arrangement).
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct VoiceResponse {
    id: Uuid,
    arrangement_id: Uuid,
    name: String,
    slug: String,
    instrument_id: Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
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

#[derive(Deserialize)]
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

/// `GET /v1/orgs/{orgId}/arrangements/{id}/voices?limit=&offset=&sort=&filter[instrumentId]=`
/// — requires at least `musician`.
async fn list_voices(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arrangement_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<VoiceListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<VoiceResponse>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;

    // Confirm the arrangement exists (and is in this org / not deleted)
    // before listing its voices, so an unknown arrangement id 404s instead
    // of silently returning an empty page.
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

/// `POST /v1/orgs/{orgId}/arrangements/{id}/voices` — owner/archivist only.
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

/// Resolve a voice and verify it belongs to `arrangement_id` **and** that the
/// arrangement belongs to `org_id`. A Voice has no `organization_id` of its
/// own, so the org check must go through the parent — without it, a caller
/// with a role in *any* org could read/modify another org's voices by id.
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

/// Same as [`find_voice_scoped`] but tolerates a soft-deleted voice (and a
/// soft-deleted parent arrangement) — used by `undelete`. The org-ownership
/// check still applies.
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

/// `GET /v1/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}` —
/// requires at least `musician`.
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

/// `PATCH /v1/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}` —
/// owner/archivist only. `slug` is immutable. Requires `If-Match`.
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

/// `DELETE /v1/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}` —
/// owner/archivist only. Soft-delete. Requires `If-Match`.
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

/// `POST /v1/orgs/{orgId}/arrangements/{arrangementId}/voices/{id}/undelete`
/// — owner/archivist only.
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
// Tag + ArrangementTag — owner/archivist gated (CLAUDE.md Permission
// matrix: "Manage tags").
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct TagResponse {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    kind: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
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

#[derive(Deserialize)]
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

/// `GET /v1/orgs/{orgId}/tags?limit=&offset=&sort=&filter[kind]=` — requires
/// at least `musician`.
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

/// `POST /v1/orgs/{orgId}/tags` — owner/archivist only.
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

/// `GET /v1/orgs/{orgId}/tags/{id}` — requires at least `musician`.
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

/// `PATCH /v1/orgs/{orgId}/tags/{id}` — owner/archivist only. Requires
/// `If-Match`.
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

/// `DELETE /v1/orgs/{orgId}/tags/{id}` — owner/archivist only. Soft-delete.
/// Requires `If-Match`.
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

/// `POST /v1/orgs/{orgId}/tags/{id}/undelete` — owner/archivist only.
async fn undelete_tag(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;

    // tag::find_by_id hides soft-deleted rows; resolve via a direct check so
    // undelete can find the very row we're restoring. Tag has no
    // "_including_deleted" finder yet (only needed here), so query it inline.
    let found = sqlx::query_as!(
        TagRow,
        r#"SELECT organization_id, name, kind FROM tag WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await
    .map_err(AppError::Database)?
    .ok_or(AppError::NotFound)?;
    if found.organization_id != org_id {
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
        serde_json::json!({ "name": found.name, "kind": found.kind }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

struct TagRow {
    organization_id: Uuid,
    name: String,
    kind: Option<String>,
}

/// `GET /v1/orgs/{orgId}/arrangements/{id}/tags` — list tags attached to an
/// arrangement. Requires at least `musician`.
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachTagRequest {
    tag_id: Uuid,
}

/// `POST /v1/orgs/{orgId}/arrangements/{id}/tags` — owner/archivist only.
/// Attaches an existing tag to the arrangement.
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

/// `DELETE /v1/orgs/{orgId}/arrangements/{arrangementId}/tags/{tagId}` —
/// owner/archivist only. Detaches a tag from the arrangement.
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
