//! `/v1/orgs`, `/v1/orgs/{id}`, `/v1/orgs/{orgId}/members`,
//! `/v1/orgs/{orgId}/members/{id}`, `/v1/users`, `/v1/users/{id}` — issue #5
//! (Org/User/Membership management).
//!
//! This module is the first to apply the full REST convention set from
//! CLAUDE.md "HTTP & REST API conventions" end to end: allowlisted sort/
//! filter (`crate::listing`), ETag/`If-Match` optimistic concurrency, and
//! `is_system_admin`/`Membership.role`-gated authorization
//! (`crate::auth::authz`). Every later phase-1 item's list/detail/mutate
//! handlers are expected to follow this same shape.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::{require_org_role_v1, require_system_admin};
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::{self, Role};
use crate::domain::{organization, user};
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
        .routes(routes!(list_orgs, create_org))
        .routes(routes!(get_org, update_org, delete_org))
        .routes(routes!(list_members, create_member))
        .routes(routes!(get_member, update_member, delete_member))
        .routes(routes!(list_users, create_user))
        .routes(routes!(get_user, delete_user))
}

// ---------------------------------------------------------------------------
// Shared list-query plumbing.
// ---------------------------------------------------------------------------

/// Raw `?limit=&offset=&sort=&filter[...]=` query params for list endpoints.
/// `limit`/`offset` are parsed by [`crate::pagination::PageParams`]; `sort`
/// is a plain string here (parsed by [`listing::resolve_sort`] against each
/// endpoint's own allowlist); `filter[...]` brackets are parsed separately
/// from the raw query string by [`listing::parse_filters`], since axum's
/// `Query` extractor doesn't natively support bracket syntax.
#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    sort: Option<String>,
}

fn validation_error(report: garde::Report) -> AppError {
    AppError::Validation(report)
}

// ---------------------------------------------------------------------------
// Organizations — system-admin-gated create/delete; open list/get/update*.
// (*update is also system-admin-gated in phase 1: no org-self-service for
// renaming, since `name` edits aren't in the Permission matrix as an owner
// capability — "Org settings" is owner-only per the matrix, so owners get it
// too; see `update_org`.)
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrganizationResponse {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<organization::Organization> for OrganizationResponse {
    fn from(o: organization::Organization) -> Self {
        Self {
            id: o.id,
            name: o.name,
            slug: o.slug,
            created_at: o.created_at,
            updated_at: o.updated_at,
        }
    }
}

/// Request body for `POST /v1/orgs`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateOrgRequest {
    name: String,
}

/// Request body for `PATCH /v1/orgs/{id}`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateOrgRequest {
    name: String,
}

/// List organizations (any authenticated identity; federation trust model).
#[utoipa::path(
    get,
    path = "/orgs",
    tag = "organizations",
    summary = "List organizations",
    params(
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort field and direction, e.g. `name:asc` (default). \
                           Allowed fields: `name`, `created_at`."),
        ("filter[name]" = Option<String>, Query,
            description = "ILIKE filter on organization name"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated organization list",
            body = inline(Page<OrganizationResponse>)),
        CommonErrors,
        Validation400,
    )
)]
async fn list_orgs(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<OrganizationResponse>>, AppError> {
    let _ = &auth;
    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        organization::SORT_ALLOWLIST,
        ("name", SortDirection::Asc),
    )
    .map_err(validation_error)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters = listing::resolve_filters(&raw_filters, organization::FILTER_ALLOWLIST)
        .map_err(validation_error)?;
    let name_filter = filters
        .iter()
        .find(|(field, _)| field == "name")
        .map(|(_, value)| value.as_str());

    let (items, total) = organization::list(
        &state.db,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        name_filter,
    )
    .await?;

    Ok(Json(Page {
        items: items.into_iter().map(OrganizationResponse::from).collect(),
        total,
        limit,
        offset,
    }))
}

/// Provision a new organization (system-admin only).
#[utoipa::path(
    post,
    path = "/orgs",
    tag = "organizations",
    summary = "Create an organization",
    security(("bearer" = []), ("session" = [])),
    request_body = CreateOrgRequest,
    responses(
        (status = 201, description = "Organization created", body = OrganizationResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        Validation400,
        Conflict409,
    )
)]
async fn create_org(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Json(body): Json<CreateOrgRequest>,
) -> Result<(StatusCode, Json<OrganizationResponse>), AppError> {
    require_system_admin(&auth.user)?;

    if body.name.trim().is_empty() {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("name"),
            garde::Error::new("name must not be empty"),
        );
        return Err(AppError::Validation(report));
    }

    let id = Uuid::now_v7();
    let slug = organization::slugify(&body.name);
    let created = organization::create(&state.db, id, &body.name, &slug, Some(auth.user.id))
        .await
        .map_err(|err| match err {
            organization::OrganizationError::DuplicateSlug => AppError::Conflict(
                "an organization with a slug derived from this name already exists".to_string(),
            ),
            organization::OrganizationError::Database(e) => AppError::Database(e),
        })?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(created.id),
            request_id: Some(request_id),
        },
        "organization.create",
        "organization",
        Some(created.id),
        serde_json::json!({ "name": created.name, "slug": created.slug }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created.into())))
}

/// Fetch one organization by ID.
#[utoipa::path(
    get,
    path = "/orgs/{id}",
    tag = "organizations",
    summary = "Get an organization",
    params(
        ("id" = Uuid, Path, description = "Organization ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Organization", body = OrganizationResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        NotFound404,
    )
)]
async fn get_org(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, AppError> {
    let _ = &auth;
    let found = organization::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    let etag = listing::etag_for(found.updated_at);
    let mut response = Json(OrganizationResponse::from(found)).into_response();
    response.headers_mut().insert(
        axum::http::header::ETAG,
        axum::http::HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("\"0\"")),
    );
    Ok(response)
}

/// Rename an organization (system-admin only; requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{id}",
    tag = "organizations",
    summary = "Update an organization",
    params(
        ("id"       = Uuid,   Path,   description = "Organization ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateOrgRequest,
    responses(
        (status = 200, description = "Updated organization", body = OrganizationResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Precondition412,
    )
)]
async fn update_org(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<UpdateOrgRequest>,
) -> Result<Json<OrganizationResponse>, AppError> {
    require_system_admin(&auth.user)?;

    let current = organization::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    let if_match = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok());
    listing::check_if_match(if_match, current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let updated = organization::update_name(&state.db, id, &body.name)
        .await?
        .ok_or(AppError::NotFound)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(id),
            request_id: Some(request_id),
        },
        "organization.update",
        "organization",
        Some(id),
        serde_json::json!({ "before": { "name": current.name }, "after": { "name": updated.name } }),
    )
    .await;

    Ok(Json(updated.into()))
}

/// Hard-delete an organization and all its data (system-admin only; requires `If-Match`).
#[utoipa::path(
    delete,
    path = "/orgs/{id}",
    tag = "organizations",
    summary = "Delete an organization",
    params(
        ("id"       = Uuid,   Path,   description = "Organization ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Organization deleted"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_org(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_system_admin(&auth.user)?;

    let current = organization::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    let if_match = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok());
    listing::check_if_match(if_match, current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(id),
            request_id: Some(request_id),
        },
        "organization.delete",
        "organization",
        Some(id),
        serde_json::json!({ "name": current.name, "slug": current.slug }),
    )
    .await;

    let deleted = organization::delete(&state.db, id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Users — system-admin-gated create/delete; open list/get (CLAUDE.md: no
// `deleted_at` on User, deletion is admin-gated hard delete).
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/users`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateUserRequest {
    username: String,
    email: Option<String>,
    display_name: String,
    password: String,
    is_system_admin: Option<bool>,
}

/// List users (system-admin only).
#[utoipa::path(
    get,
    path = "/users",
    tag = "users",
    summary = "List users",
    params(
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort field and direction, e.g. `username:asc` (default). \
                           Allowed fields: `username`, `created_at`."),
        ("filter[username]" = Option<String>, Query,
            description = "ILIKE filter on username"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated user list",
            body = inline(Page<user::User>)),
        CommonErrors,
        Forbidden403,
        Validation400,
    )
)]
async fn list_users(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<user::User>>, AppError> {
    require_system_admin(&auth.user)?;

    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        user::SORT_ALLOWLIST,
        ("username", SortDirection::Asc),
    )
    .map_err(validation_error)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters =
        listing::resolve_filters(&raw_filters, user::FILTER_ALLOWLIST).map_err(validation_error)?;
    let username_filter = filters
        .iter()
        .find(|(field, _)| field == "username")
        .map(|(_, value)| value.as_str());

    let (items, total) = user::list(
        &state.db,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        username_filter,
    )
    .await?;

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Provision a new user account (system-admin only).
#[utoipa::path(
    post,
    path = "/users",
    tag = "users",
    summary = "Create a user",
    security(("bearer" = []), ("session" = [])),
    request_body = CreateUserRequest,
    responses(
        (status = 201, description = "User created", body = user::User,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        Validation400,
        Conflict409,
    )
)]
async fn create_user(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Json(body): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<user::User>), AppError> {
    require_system_admin(&auth.user)?;

    if body.username.trim().is_empty() || body.password.is_empty() {
        let mut report = garde::Report::new();
        if body.username.trim().is_empty() {
            report.append(
                garde::Path::new("username"),
                garde::Error::new("username must not be empty"),
            );
        }
        if body.password.is_empty() {
            report.append(
                garde::Path::new("password"),
                garde::Error::new("password must not be empty"),
            );
        }
        return Err(AppError::Validation(report));
    }

    let password_hash = crate::auth::password::hash_password(&body.password)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("failed to hash password")))?;
    let id = Uuid::now_v7();
    let slug = user::slugify(&body.username);

    let created = user::create(
        &state.db,
        id,
        &slug,
        &body.username,
        body.email.as_deref(),
        Some(&password_hash),
        &body.display_name,
        body.is_system_admin.unwrap_or(false),
        Some(auth.user.id),
    )
    .await
    .map_err(|err| match err {
        user::UserError::DuplicateUsername => {
            AppError::Conflict("a user with this username already exists".to_string())
        }
        user::UserError::Database(e) => AppError::Database(e),
    })?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: None,
            request_id: Some(request_id),
        },
        "user.create",
        "user",
        Some(created.id),
        serde_json::json!({ "username": created.username, "slug": created.slug, "isSystemAdmin": created.is_system_admin }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created)))
}

/// Fetch one user by ID (system-admin only).
#[utoipa::path(
    get,
    path = "/users/{id}",
    tag = "users",
    summary = "Get a user",
    params(
        ("id" = Uuid, Path, description = "User ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "User", body = user::User,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_user(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, AppError> {
    require_system_admin(&auth.user)?;

    let found = user::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    let etag = listing::etag_for(found.updated_at);
    let mut response = Json(found).into_response();
    response.headers_mut().insert(
        axum::http::header::ETAG,
        axum::http::HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("\"0\"")),
    );
    Ok(response)
}

/// Hard-delete a user (system-admin only; requires `If-Match`).
///
/// Fails with `409` if the user is still referenced by a membership, part
/// assignment, or annotation — reassign or remove those first.
#[utoipa::path(
    delete,
    path = "/users/{id}",
    tag = "users",
    summary = "Delete a user",
    params(
        ("id"       = Uuid,   Path,   description = "User ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "User deleted"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Conflict409,
        Precondition412,
    )
)]
async fn delete_user(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_system_admin(&auth.user)?;

    let current = user::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    let if_match = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok());
    listing::check_if_match(if_match, current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let deleted = user::delete(&state.db, id).await.map_err(|err| {
        if let sqlx::Error::Database(ref db_err) = err {
            if db_err.is_foreign_key_violation() {
                return AppError::Conflict(
                    "this user is still referenced by a membership, part assignment, or annotation; reassign or remove those first".to_string(),
                );
            }
        }
        AppError::Database(err)
    })?;
    if !deleted {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: None,
            request_id: Some(request_id),
        },
        "user.delete",
        "user",
        Some(id),
        serde_json::json!({ "username": current.username, "slug": current.slug }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Memberships — owner-gated manage; member listing requires at least
// `musician` (i.e. any member of the org) to read.
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MembershipResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub organization_id: Uuid,
    pub role: Role,
    pub instrument_ids: Vec<Uuid>,
    pub is_principal: bool,
    pub principal_instrument_ids: Vec<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<membership::Membership> for MembershipResponse {
    fn from(m: membership::Membership) -> Self {
        Self {
            id: m.id,
            user_id: m.user_id,
            organization_id: m.organization_id,
            role: m.role,
            instrument_ids: m.instrument_ids,
            is_principal: m.is_principal,
            principal_instrument_ids: m.principal_instrument_ids,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

/// Request body for `POST /v1/orgs/{orgId}/members`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateMemberRequest {
    user_id: Uuid,
    role: String,
    instrument_ids: Option<Vec<Uuid>>,
    is_principal: Option<bool>,
    principal_instrument_ids: Option<Vec<Uuid>>,
}

/// Request body for `PATCH /v1/orgs/{orgId}/members/{id}`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateMemberRequest {
    role: String,
    instrument_ids: Option<Vec<Uuid>>,
    is_principal: Option<bool>,
    principal_instrument_ids: Option<Vec<Uuid>>,
}

/// List memberships in an organization (requires `musician` role or system-admin).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/members",
    tag = "members",
    summary = "List members",
    params(
        ("orgId"  = Uuid, Path, description = "Organization ID"),
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort field and direction. Allowed: `created_at` (default), `role`."),
        ("filter[role]" = Option<String>, Query,
            description = "Filter by role: `owner`, `archivist`, `conductor`, `musician`"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated membership list",
            body = inline(Page<MembershipResponse>)),
        CommonErrors,
        Forbidden403,
        Validation400,
    )
)]
async fn list_members(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path(org_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
    raw_uri: axum::http::Uri,
) -> Result<Json<Page<MembershipResponse>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;

    let (limit, offset) = crate::pagination::PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        membership::SORT_ALLOWLIST,
        ("created_at", SortDirection::Asc),
    )
    .map_err(validation_error)?;

    let raw_filters = listing::parse_filters(raw_uri.query().unwrap_or(""));
    let filters = listing::resolve_filters(&raw_filters, membership::FILTER_ALLOWLIST)
        .map_err(validation_error)?;
    let role_filter = filters
        .iter()
        .find(|(field, _)| field == "role")
        .map(|(_, value)| value.as_str());

    let (items, total) = membership::list_for_org(
        &state.db,
        org_id,
        i64::from(limit),
        i64::from(offset),
        sort_column,
        sort_direction,
        role_filter,
    )
    .await?;

    Ok(Json(Page {
        items: items.into_iter().map(MembershipResponse::from).collect(),
        total,
        limit,
        offset,
    }))
}

/// Add a user to an organization (owner only).
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/members",
    tag = "members",
    summary = "Add a member",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = CreateMemberRequest,
    responses(
        (status = 201, description = "Membership created", body = MembershipResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        Validation400,
        Conflict409,
        NotFound404,
    )
)]
async fn create_member(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(org_id): Path<Uuid>,
    Json(body): Json<CreateMemberRequest>,
) -> Result<(StatusCode, Json<MembershipResponse>), AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Owner).await?;

    let role = parse_role_or_validation_error(&body.role)?;
    let instrument_ids = body.instrument_ids.unwrap_or_default();
    let principal_instrument_ids = body.principal_instrument_ids.unwrap_or_default();
    let is_principal = body.is_principal.unwrap_or(false);

    let id = Uuid::now_v7();
    let created = membership::create(
        &state.db,
        id,
        body.user_id,
        org_id,
        membership::MembershipFields {
            role,
            instrument_ids: &instrument_ids,
            is_principal,
            principal_instrument_ids: &principal_instrument_ids,
        },
        Some(auth.user.id),
    )
    .await
    .map_err(membership_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "membership.create",
        "membership",
        Some(created.id),
        serde_json::json!({
            "userId": created.user_id,
            "role": created.role,
            "instrumentIds": created.instrument_ids,
            "isPrincipal": created.is_principal,
            "principalInstrumentIds": created.principal_instrument_ids,
        }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(created.into())))
}

fn membership_error_to_app_error(err: membership::MembershipError) -> AppError {
    match err {
        membership::MembershipError::Database(e) => AppError::Database(e),
        membership::MembershipError::Duplicate => {
            AppError::Conflict("this user is already a member of this organization".to_string())
        }
        membership::MembershipError::UnknownInstrument => {
            let mut report = garde::Report::new();
            report.append(
                garde::Path::new("instrumentIds"),
                garde::Error::new("one or more instrument ids do not reference a live instrument"),
            );
            AppError::Validation(report)
        }
        membership::MembershipError::PrincipalNotSubset => {
            let mut report = garde::Report::new();
            report.append(
                garde::Path::new("principalInstrumentIds"),
                garde::Error::new("principalInstrumentIds must be a subset of instrumentIds"),
            );
            AppError::Validation(report)
        }
        membership::MembershipError::LastOwner => AppError::Conflict(
            "organization must keep at least one owner; demote or remove a different owner first"
                .to_string(),
        ),
    }
}

fn parse_role_or_validation_error(raw: &str) -> Result<Role, AppError> {
    Role::parse(raw).ok_or_else(|| {
        let mut report = garde::Report::new();
        report.append(
            garde::Path::new("role"),
            garde::Error::new("role must be one of: owner, archivist, conductor, musician"),
        );
        AppError::Validation(report)
    })
}

/// Look up a membership and verify it belongs to `org_id` (path scoping
/// consistency — a membership id from a different org must 404, not leak
/// cross-org existence via a 200/403 split).
async fn find_member_scoped(
    state: &AppState,
    org_id: Uuid,
    id: Uuid,
) -> Result<membership::Membership, AppError> {
    let found = membership::find_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if found.organization_id != org_id {
        return Err(AppError::NotFound);
    }
    Ok(found)
}

/// Fetch one membership by ID (requires `musician` role or system-admin).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/members/{id}",
    tag = "members",
    summary = "Get a member",
    params(
        ("orgId" = Uuid, Path, description = "Organization ID"),
        ("id"    = Uuid, Path, description = "Membership ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Membership", body = MembershipResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn get_member(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<axum::response::Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;

    let found = find_member_scoped(&state, org_id, id).await?;
    let etag = listing::etag_for(found.updated_at);
    let mut response = Json(MembershipResponse::from(found)).into_response();
    response.headers_mut().insert(
        axum::http::header::ETAG,
        axum::http::HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("\"0\"")),
    );
    Ok(response)
}

/// Update a member's role or instruments (owner only; requires `If-Match`).
#[utoipa::path(
    patch,
    path = "/orgs/{orgId}/members/{id}",
    tag = "members",
    summary = "Update a member",
    params(
        ("orgId"   = Uuid,   Path,   description = "Organization ID"),
        ("id"      = Uuid,   Path,   description = "Membership ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body = UpdateMemberRequest,
    responses(
        (status = 200, description = "Updated membership", body = MembershipResponse,
            headers(("ETag" = String, description = "updated_at ms-epoch, quoted"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Validation400,
        Conflict409,
        Precondition412,
    )
)]
async fn update_member(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(body): Json<UpdateMemberRequest>,
) -> Result<Json<MembershipResponse>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Owner).await?;

    let current = find_member_scoped(&state, org_id, id).await?;

    let if_match = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok());
    listing::check_if_match(if_match, current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    let role = parse_role_or_validation_error(&body.role)?;
    let instrument_ids = body
        .instrument_ids
        .unwrap_or_else(|| current.instrument_ids.clone());
    let principal_instrument_ids = body
        .principal_instrument_ids
        .unwrap_or_else(|| current.principal_instrument_ids.clone());
    let is_principal = body.is_principal.unwrap_or(current.is_principal);

    let updated = membership::update(
        &state.db,
        id,
        membership::MembershipFields {
            role,
            instrument_ids: &instrument_ids,
            is_principal,
            principal_instrument_ids: &principal_instrument_ids,
        },
    )
    .await
    .map_err(membership_error_to_app_error)?
    .ok_or(AppError::NotFound)?;

    let action = if current.role != updated.role {
        "membership.role_change"
    } else if current.is_principal != updated.is_principal {
        "membership.principal_change"
    } else {
        "membership.update"
    };

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        action,
        "membership",
        Some(id),
        serde_json::json!({
            "before": {
                "role": current.role,
                "instrumentIds": current.instrument_ids,
                "isPrincipal": current.is_principal,
                "principalInstrumentIds": current.principal_instrument_ids,
            },
            "after": {
                "role": updated.role,
                "instrumentIds": updated.instrument_ids,
                "isPrincipal": updated.is_principal,
                "principalInstrumentIds": updated.principal_instrument_ids,
            },
        }),
    )
    .await;

    Ok(Json(updated.into()))
}

/// Remove a member from an organization (owner only; requires `If-Match`).
///
/// Fails with `409` if this is the org's last owner.
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/members/{id}",
    tag = "members",
    summary = "Remove a member",
    params(
        ("orgId"   = Uuid,   Path,   description = "Organization ID"),
        ("id"      = Uuid,   Path,   description = "Membership ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "Member removed"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Conflict409,
        Precondition412,
    )
)]
async fn delete_member(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Owner).await?;

    let current = find_member_scoped(&state, org_id, id).await?;

    let if_match = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok());
    listing::check_if_match(if_match, current.updated_at)
        .map_err(|_| AppError::PreconditionFailed)?;

    membership::delete(&state.db, id)
        .await
        .map_err(membership_error_to_app_error)?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "membership.delete",
        "membership",
        Some(id),
        serde_json::json!({
            "userId": current.user_id,
            "role": current.role,
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}
