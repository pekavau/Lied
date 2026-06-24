//! `/admin/orgs`, `/admin/orgs/{id}`, `/admin/orgs/{id}/members`,
//! `/admin/users` — HTMX admin screens for issue #5 (Org/User/Membership
//! management). Minimal but functional: list + create/edit/delete forms,
//! server-rendered HTML fragments per CLAUDE.md's HTMX admin-UI convention.
//!
//! Authorization mirrors the `/v1` handlers in [`crate::routes::orgs`]: org
//! create/delete and user create/delete are system-admin-gated; membership
//! management is owner-gated via [`crate::auth::authz::require_org_role`].
//! A caller who fails a check gets a small HTML error fragment (not Problem
//! Details — the HTMX tree is exempt from RFC 7807 per CLAUDE.md) with the
//! same status code.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Router};
use maud::html;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::authz::{require_org_role, require_system_admin};
use crate::auth::extractors::AuthSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::{self, Role};
use crate::domain::{organization, user};
use crate::error::AppError;
use crate::listing::SortDirection;
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs", get(list_orgs_page).post(create_org_submit))
        .route("/orgs/:id", get(org_detail_page))
        .route("/orgs/:id/delete", axum::routing::post(delete_org_submit))
        .route(
            "/orgs/:org_id/members",
            axum::routing::post(create_member_submit),
        )
        .route(
            "/orgs/:org_id/members/:id/delete",
            axum::routing::post(delete_member_submit),
        )
        .route(
            "/orgs/:org_id/members/:id/role",
            axum::routing::post(update_member_role_submit),
        )
        .route("/users", get(list_users_page).post(create_user_submit))
        .route("/users/:id/delete", axum::routing::post(delete_user_submit))
}

/// Render a small HTML error fragment carrying `status`, for use by every
/// handler in this module — the `/admin` tree's equivalent of `AppError`'s
/// Problem Details (CLAUDE.md: "HTMX error responses are HTML fragments,
/// not Problem Details").
fn error_fragment(status: StatusCode, message: &str) -> Response {
    (status, Html(format!("<p class=\"error\">{message}</p>"))).into_response()
}

fn app_error_fragment(err: AppError) -> Response {
    let status = match &err {
        AppError::NotFound => StatusCode::NOT_FOUND,
        AppError::Forbidden => StatusCode::FORBIDDEN,
        AppError::Unauthorized => StatusCode::UNAUTHORIZED,
        AppError::Conflict(_) => StatusCode::CONFLICT,
        AppError::Validation(_) => StatusCode::BAD_REQUEST,
        AppError::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_fragment(status, &err.to_string())
}

// ---------------------------------------------------------------------------
// Organizations
// ---------------------------------------------------------------------------

/// `GET /admin/orgs` — list + create form. System-admin-only (mirrors the
/// `/v1` gate): a musician/archivist has no UI need to see the instance-wide
/// org directory in the admin tree.
async fn list_orgs_page(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
) -> Response {
    if let Err(err) = require_system_admin(&actor) {
        return app_error_fragment(err);
    }

    let (orgs, _total) =
        match organization::list(&state.db, 200, 0, "name", SortDirection::Asc, None).await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(%error, "failed to list organizations");
                return error_fragment(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to load organizations",
                );
            }
        };

    let body = html! {
        form method="post" action="/admin/orgs" {
            fieldset {
                legend { "Create organization" }
                label { "Name " input type="text" name="name" required; }
                button type="submit" { "Create" }
            }
        }
        table {
            thead { tr { th { "Name" } th { "Slug" } th { "" } } }
            tbody {
                @for org in &orgs {
                    tr {
                        td { a href={"/admin/orgs/" (org.id)} { (org.name) } }
                        td { (org.slug) }
                        td {
                            form class="inline" method="post" action={"/admin/orgs/" (org.id) "/delete"} {
                                button type="submit" { "Delete" }
                            }
                        }
                    }
                }
            }
        }
    };

    Html(layout::page("Organizations", &actor.display_name, body).into_string()).into_response()
}

#[derive(Deserialize)]
struct CreateOrgForm {
    name: String,
}

/// `POST /admin/orgs` — create an organization (system-admin-only).
async fn create_org_submit(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Form(form): Form<CreateOrgForm>,
) -> Response {
    if let Err(err) = require_system_admin(&actor) {
        return app_error_fragment(err);
    }

    if form.name.trim().is_empty() {
        return error_fragment(StatusCode::BAD_REQUEST, "Name must not be empty.");
    }

    let id = Uuid::now_v7();
    let slug = organization::slugify(&form.name);
    match organization::create(&state.db, id, &form.name, &slug, Some(actor.id)).await {
        Ok(created) => {
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(actor.id),
                    org_id: Some(created.id),
                    request_id: Some(request_id),
                },
                "organization.create",
                "organization",
                Some(created.id),
                serde_json::json!({ "name": created.name, "slug": created.slug }),
            )
            .await;
            Redirect::to("/admin/orgs").into_response()
        }
        Err(organization::OrganizationError::DuplicateSlug) => error_fragment(
            StatusCode::CONFLICT,
            "An organization with this name already exists.",
        ),
        Err(organization::OrganizationError::Database(error)) => {
            tracing::error!(%error, "failed to create organization");
            error_fragment(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to create organization",
            )
        }
    }
}

/// `POST /admin/orgs/{id}/delete` — hard-delete an organization
/// (system-admin-only). No `If-Match` in the admin UI: this form has no
/// ETag to carry, and the HTMX tree isn't bound by the `/v1` optimistic-
/// concurrency contract (CLAUDE.md scopes that convention to the JSON tree).
async fn delete_org_submit(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(err) = require_system_admin(&actor) {
        return app_error_fragment(err);
    }

    let current = match organization::find_by_id(&state.db, id).await {
        Ok(Some(org)) => org,
        Ok(None) => return error_fragment(StatusCode::NOT_FOUND, "Organization not found."),
        Err(error) => {
            tracing::error!(%error, "failed to look up organization");
            return error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "lookup failed");
        }
    };

    match organization::delete(&state.db, id).await {
        Ok(true) => {
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(actor.id),
                    org_id: Some(id),
                    request_id: Some(request_id),
                },
                "organization.delete",
                "organization",
                Some(id),
                serde_json::json!({ "name": current.name, "slug": current.slug }),
            )
            .await;
            Redirect::to("/admin/orgs").into_response()
        }
        Ok(false) => error_fragment(StatusCode::NOT_FOUND, "Organization not found."),
        Err(error) => {
            tracing::error!(%error, "failed to delete organization");
            error_fragment(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to delete organization",
            )
        }
    }
}

/// `GET /admin/orgs/{id}` — org detail + member roster + add-member form.
/// Requires at least `musician` membership in the org (or system admin),
/// matching the `/v1` member-list gate.
async fn org_detail_page(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(err) = require_org_role(&state, &actor, id, Role::Musician).await {
        return app_error_fragment(err);
    }
    let is_owner = require_org_role(&state, &actor, id, Role::Owner)
        .await
        .is_ok();

    let org = match organization::find_by_id(&state.db, id).await {
        Ok(Some(org)) => org,
        Ok(None) => return error_fragment(StatusCode::NOT_FOUND, "Organization not found."),
        Err(error) => {
            tracing::error!(%error, "failed to look up organization");
            return error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "lookup failed");
        }
    };

    let (members, _total) = match membership::list_for_org(
        &state.db,
        id,
        200,
        0,
        "created_at",
        SortDirection::Asc,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "failed to list members");
            return error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "failed to load members");
        }
    };

    // Best-effort username lookup per member for display; phase-1 admin UI
    // accepts N+1 lookups here for simplicity at the scale this targets
    // (small orgs) rather than adding a join helper for one screen.
    let mut rows = Vec::with_capacity(members.len());
    for m in &members {
        let username = match user::find_by_id(&state.db, m.user_id).await {
            Ok(Some(u)) => u.username,
            _ => m.user_id.to_string(),
        };
        rows.push((m.clone(), username));
    }

    let body = html! {
        p { "Slug: " (org.slug) }
        h2 { "Members" }
        table {
            thead { tr { th { "User" } th { "Role" } th { "Principal" } th { "" } } }
            tbody {
                @for (m, username) in &rows {
                    tr {
                        td { (username) }
                        td {
                            @if is_owner {
                                form class="inline" method="post" action={"/admin/orgs/" (id) "/members/" (m.id) "/role"} {
                                    select name="role" {
                                        @for role in [Role::Owner, Role::Archivist, Role::Conductor, Role::Musician] {
                                            option value=(role.as_str()) selected[role == m.role] { (role.as_str()) }
                                        }
                                    }
                                    button type="submit" { "Update" }
                                }
                            } @else {
                                (m.role.as_str())
                            }
                        }
                        td { @if m.is_principal { "yes" } @else { "no" } }
                        td {
                            @if is_owner {
                                form class="inline" method="post" action={"/admin/orgs/" (id) "/members/" (m.id) "/delete"} {
                                    button type="submit" { "Remove" }
                                }
                            }
                        }
                    }
                }
            }
        }
        @if is_owner {
            h2 { "Add member" }
            form method="post" action={"/admin/orgs/" (id) "/members"} {
                fieldset {
                    label { "User ID (UUID) " input type="text" name="user_id" required; }
                    label {
                        "Role "
                        select name="role" {
                            @for role in [Role::Owner, Role::Archivist, Role::Conductor, Role::Musician] {
                                option value=(role.as_str()) { (role.as_str()) }
                            }
                        }
                    }
                    button type="submit" { "Add" }
                }
            }
        }
    };

    Html(
        layout::page(
            &format!("Organization: {}", org.name),
            &actor.display_name,
            body,
        )
        .into_string(),
    )
    .into_response()
}

#[derive(Deserialize)]
struct CreateMemberForm {
    user_id: Uuid,
    role: String,
}

/// `POST /admin/orgs/{orgId}/members` — add a member (owner-only).
async fn create_member_submit(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(org_id): Path<Uuid>,
    Form(form): Form<CreateMemberForm>,
) -> Response {
    if let Err(err) = require_org_role(&state, &actor, org_id, Role::Owner).await {
        return app_error_fragment(err);
    }

    let Some(role) = Role::parse(&form.role) else {
        return error_fragment(StatusCode::BAD_REQUEST, "Unknown role.");
    };

    let id = Uuid::now_v7();
    match membership::create(
        &state.db,
        id,
        form.user_id,
        org_id,
        membership::MembershipFields {
            role,
            instrument_ids: &[],
            is_principal: false,
            principal_instrument_ids: &[],
        },
        Some(actor.id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(actor.id),
                    org_id: Some(org_id),
                    request_id: Some(request_id),
                },
                "membership.create",
                "membership",
                Some(created.id),
                serde_json::json!({ "userId": created.user_id, "role": created.role }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}")).into_response()
        }
        Err(err) => membership_error_fragment(err),
    }
}

#[derive(Deserialize)]
struct UpdateMemberRoleForm {
    role: String,
}

/// `POST /admin/orgs/{orgId}/members/{id}/role` — change a member's role
/// (owner-only). Last-owner protection is enforced by
/// [`membership::update`] itself.
async fn update_member_role_submit(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    Form(form): Form<UpdateMemberRoleForm>,
) -> Response {
    if let Err(err) = require_org_role(&state, &actor, org_id, Role::Owner).await {
        return app_error_fragment(err);
    }

    let Some(role) = Role::parse(&form.role) else {
        return error_fragment(StatusCode::BAD_REQUEST, "Unknown role.");
    };

    let current = match membership::find_by_id(&state.db, id).await {
        Ok(Some(m)) if m.organization_id == org_id => m,
        Ok(_) => return error_fragment(StatusCode::NOT_FOUND, "Membership not found."),
        Err(error) => {
            tracing::error!(%error, "failed to look up membership");
            return error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "lookup failed");
        }
    };

    match membership::update(
        &state.db,
        id,
        membership::MembershipFields {
            role,
            instrument_ids: &current.instrument_ids,
            is_principal: current.is_principal,
            principal_instrument_ids: &current.principal_instrument_ids,
        },
    )
    .await
    {
        Ok(Some(updated)) => {
            let action = if current.role != updated.role {
                "membership.role_change"
            } else {
                "membership.update"
            };
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(actor.id),
                    org_id: Some(org_id),
                    request_id: Some(request_id),
                },
                action,
                "membership",
                Some(id),
                serde_json::json!({ "before": { "role": current.role }, "after": { "role": updated.role } }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}")).into_response()
        }
        Ok(None) => error_fragment(StatusCode::NOT_FOUND, "Membership not found."),
        Err(err) => membership_error_fragment(err),
    }
}

/// `POST /admin/orgs/{orgId}/members/{id}/delete` — remove a member
/// (owner-only). Last-owner protection is enforced by [`membership::delete`].
async fn delete_member_submit(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(err) = require_org_role(&state, &actor, org_id, Role::Owner).await {
        return app_error_fragment(err);
    }

    let current = match membership::find_by_id(&state.db, id).await {
        Ok(Some(m)) if m.organization_id == org_id => m,
        Ok(_) => return error_fragment(StatusCode::NOT_FOUND, "Membership not found."),
        Err(error) => {
            tracing::error!(%error, "failed to look up membership");
            return error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "lookup failed");
        }
    };

    match membership::delete(&state.db, id).await {
        Ok(_) => {
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(actor.id),
                    org_id: Some(org_id),
                    request_id: Some(request_id),
                },
                "membership.delete",
                "membership",
                Some(id),
                serde_json::json!({ "userId": current.user_id, "role": current.role }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}")).into_response()
        }
        Err(err) => membership_error_fragment(err),
    }
}

fn membership_error_fragment(err: membership::MembershipError) -> Response {
    match err {
        membership::MembershipError::Database(error) => {
            tracing::error!(%error, "membership operation failed");
            error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "operation failed")
        }
        membership::MembershipError::Duplicate => {
            error_fragment(StatusCode::CONFLICT, "This user is already a member.")
        }
        membership::MembershipError::UnknownInstrument => {
            error_fragment(StatusCode::BAD_REQUEST, "Unknown instrument id.")
        }
        membership::MembershipError::PrincipalNotSubset => error_fragment(
            StatusCode::BAD_REQUEST,
            "Principal instruments must be a subset of assigned instruments.",
        ),
        membership::MembershipError::LastOwner => error_fragment(
            StatusCode::CONFLICT,
            "Organization must keep at least one owner.",
        ),
    }
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

/// `GET /admin/users` — list + create form (system-admin-only).
async fn list_users_page(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
) -> Response {
    if let Err(err) = require_system_admin(&actor) {
        return app_error_fragment(err);
    }

    let (users, _total) =
        match user::list(&state.db, 200, 0, "username", SortDirection::Asc, None).await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(%error, "failed to list users");
                return error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "failed to load users");
            }
        };

    let body = html! {
        form method="post" action="/admin/users" {
            fieldset {
                legend { "Create user" }
                label { "Username " input type="text" name="username" required; }
                label { "Display name " input type="text" name="display_name" required; }
                label { "Email " input type="email" name="email"; }
                label { "Password " input type="password" name="password" required; }
                label { "System admin " input type="checkbox" name="is_system_admin" value="true"; }
                button type="submit" { "Create" }
            }
        }
        table {
            thead { tr { th { "Username" } th { "Display name" } th { "System admin" } th { "" } } }
            tbody {
                @for u in &users {
                    tr {
                        td { (u.username) }
                        td { (u.display_name) }
                        td { @if u.is_system_admin { "yes" } @else { "no" } }
                        td {
                            form class="inline" method="post" action={"/admin/users/" (u.id) "/delete"} {
                                button type="submit" { "Delete" }
                            }
                        }
                    }
                }
            }
        }
    };

    Html(layout::page("Users", &actor.display_name, body).into_string()).into_response()
}

#[derive(Deserialize)]
struct CreateUserForm {
    username: String,
    display_name: String,
    email: Option<String>,
    password: String,
    is_system_admin: Option<String>,
}

/// `POST /admin/users` — create a user (system-admin-only).
async fn create_user_submit(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Form(form): Form<CreateUserForm>,
) -> Response {
    if let Err(err) = require_system_admin(&actor) {
        return app_error_fragment(err);
    }

    if form.username.trim().is_empty() || form.password.is_empty() {
        return error_fragment(
            StatusCode::BAD_REQUEST,
            "Username and password are required.",
        );
    }

    let password_hash = match crate::auth::password::hash_password(&form.password) {
        Ok(hash) => hash,
        Err(error) => {
            tracing::error!(?error, "failed to hash password");
            return error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "failed to create user");
        }
    };

    let id = Uuid::now_v7();
    let slug = user::slugify(&form.username);
    let is_system_admin = form.is_system_admin.as_deref() == Some("true");
    let email = form.email.filter(|e| !e.trim().is_empty());

    match user::create(
        &state.db,
        id,
        &slug,
        &form.username,
        email.as_deref(),
        Some(&password_hash),
        &form.display_name,
        is_system_admin,
        Some(actor.id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(actor.id),
                    org_id: None,
                    request_id: Some(request_id),
                },
                "user.create",
                "user",
                Some(created.id),
                serde_json::json!({ "username": created.username, "isSystemAdmin": created.is_system_admin }),
            )
            .await;
            Redirect::to("/admin/users").into_response()
        }
        Err(user::UserError::DuplicateUsername) => {
            error_fragment(StatusCode::CONFLICT, "This username already exists.")
        }
        Err(user::UserError::Database(error)) => {
            tracing::error!(%error, "failed to create user");
            error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "failed to create user")
        }
    }
}

/// `POST /admin/users/{id}/delete` — hard-delete a user (system-admin-only).
/// A user still referenced by a `NOT NULL` FK (membership, part assignment,
/// annotation authorship) surfaces as a `409`, matching the `/v1` behavior
/// (see [`user::delete`] doc comment).
async fn delete_user_submit(
    AuthSession(actor): AuthSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(err) = require_system_admin(&actor) {
        return app_error_fragment(err);
    }

    let current = match user::find_by_id(&state.db, id).await {
        Ok(Some(u)) => u,
        Ok(None) => return error_fragment(StatusCode::NOT_FOUND, "User not found."),
        Err(error) => {
            tracing::error!(%error, "failed to look up user");
            return error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "lookup failed");
        }
    };

    match user::delete(&state.db, id).await {
        Ok(true) => {
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(actor.id),
                    org_id: None,
                    request_id: Some(request_id),
                },
                "user.delete",
                "user",
                Some(id),
                serde_json::json!({ "username": current.username }),
            )
            .await;
            Redirect::to("/admin/users").into_response()
        }
        Ok(false) => error_fragment(StatusCode::NOT_FOUND, "User not found."),
        Err(error) => {
            if let sqlx::Error::Database(ref db_err) = error {
                if db_err.is_foreign_key_violation() {
                    return error_fragment(
                        StatusCode::CONFLICT,
                        "This user is still referenced by a membership, part assignment, or annotation.",
                    );
                }
            }
            tracing::error!(%error, "failed to delete user");
            error_fragment(StatusCode::INTERNAL_SERVER_ERROR, "failed to delete user")
        }
    }
}
