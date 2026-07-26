//! `/admin/orgs/{org}/members` — the owner-facing member & instrument-config
//! console (Phase 2, issue #31, slice 4). The single member-management
//! surface: the system-admin org-detail page is now a read-only roster.
//!
//! Authorization: owner only (matrix "Manage members & roles" =
//! [`console::ConsoleCtx::can_manage_members`], with `is_system_admin`
//! owner-equivalent).
//!
//! Members are added **by username** (not a raw UUID) — this both fixes the
//! discovery/UX gap (issue #41) and removes the extractor-level UUID parse
//! failure that leaked a raw axum error.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::{html, Markup};
use serde::Deserialize;
use tower_sessions::Session;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::audit_log::audit;
use crate::domain::membership::{self, Membership, Role};
use crate::domain::{instrument, user};
use crate::listing::{check_if_match, SortDirection};
use crate::routes::admin::arrangements::{
    audit_ctx, blank_to_none, csrf_token, instrument_name, load_instruments, ordered_families,
    require_found,
};
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs/:org_id/members", get(members_page).post(add_member))
        .route(
            "/orgs/:org_id/members/:membership_id",
            get(member_edit_page).post(update_member),
        )
        .route(
            "/orgs/:org_id/members/:membership_id/delete",
            post(remove_member),
        )
}

/// Full-page error inside the Members section shell (thin wrapper over the
/// shared [`console::error_page`], pinned to this section for the nav highlight).
fn error_page(ctx: &ConsoleCtx, status: StatusCode, message: &str) -> Response {
    console::error_page(ctx, Section::Members, status, message)
}

// ---------------------------------------------------------------------------
// Members list + add-by-username
// ---------------------------------------------------------------------------

async fn members_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    session: Session,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_manage_members() {
        return console::section_forbidden(&ctx);
    }
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };

    let (members, _total) = match membership::list_for_org(
        &state.db,
        org_id,
        500,
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
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load members.",
            );
        }
    };
    let instruments = load_instruments(&state).await.unwrap_or_default();

    // Username per member for display (small orgs — N+1 is fine here).
    let mut rows = Vec::with_capacity(members.len());
    for m in &members {
        let username = match user::find_by_id(&state.db, m.user_id).await {
            Ok(Some(u)) => u.username,
            _ => m.user_id.to_string(),
        };
        rows.push((m.clone(), username));
    }

    let body = html! {
        table {
            thead { tr { th { "User" } th { "Role" } th { "Instruments" } th { "Principal" } th {} } }
            tbody {
                @for (m, username) in &rows {
                    tr {
                        td { (username) }
                        td { (m.role.as_str()) }
                        td {
                            @if m.instrument_ids.is_empty() { span class="muted" { "—" } }
                            @else {
                                @for (i, id) in m.instrument_ids.iter().enumerate() {
                                    @if i > 0 { ", " }
                                    (instrument_name(&instruments, *id))
                                }
                            }
                        }
                        td { @if m.is_principal { "yes" } @else { "no" } }
                        td {
                            a href=(format!("/admin/orgs/{org_id}/members/{}", m.id)) { "Edit" }
                            " "
                            form class="inline" method="post" action=(format!("/admin/orgs/{org_id}/members/{}/delete", m.id)) {
                                (layout::csrf_field(&token))
                                button type="submit" { "Remove" }
                            }
                        }
                    }
                }
            }
        }
        @if rows.is_empty() { p class="muted" { "No members yet." } }

        h2 { "Add member" }
        form method="post" action=(format!("/admin/orgs/{org_id}/members")) {
            (layout::csrf_field(&token))
            label { "Username " input type="text" name="username" required; }
            label {
                "Role "
                select name="role" {
                    @for role in [Role::Owner, Role::Archivist, Role::Conductor, Role::Musician] {
                        option value=(role.as_str()) selected[role == Role::Musician] { (role.as_str()) }
                    }
                }
            }
            button type="submit" { "Add member" }
        }
        p class="muted" { "Set a member's instruments and principal status via Edit." }
    };
    Html(console::console_page(&ctx, Section::Members, body).into_string()).into_response()
}

#[derive(Deserialize)]
struct AddMemberForm {
    username: Option<String>,
    role: Option<String>,
}

async fn add_member(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    RequestId(request_id): RequestId,
    Form(form): Form<AddMemberForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_manage_members() {
        return console::section_forbidden(&ctx);
    }
    let Some(username) = blank_to_none(form.username) else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Username must not be empty.");
    };
    let Some(role) = form.role.as_deref().and_then(Role::parse) else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Please choose a valid role.");
    };

    // Resolve the username → user (this is the #41 fix: no raw UUID in the form).
    let target = match user::find_by_username(&state.db, &username).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return error_page(
                &ctx,
                StatusCode::NOT_FOUND,
                &format!("No user with username \"{username}\"."),
            )
        }
        Err(error) => {
            tracing::error!(%error, "failed to look up user by username");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not look up that user.",
            );
        }
    };

    let fields = membership::MembershipFields {
        role,
        instrument_ids: &[],
        is_principal: false,
        principal_instrument_ids: &[],
    };
    match membership::create(
        &state.db,
        Uuid::now_v7(),
        target.id,
        org_id,
        fields,
        Some(ctx.user().id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "membership.create",
                "membership",
                Some(created.id),
                serde_json::json!({ "userId": target.id, "role": role.as_str() }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/members")).into_response()
        }
        Err(membership::MembershipError::Duplicate) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            &format!("\"{username}\" is already a member of this organization."),
        ),
        Err(error) => {
            tracing::error!(%error, "failed to add member");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not add the member.",
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Per-member edit (role + instruments + principal) and remove
// ---------------------------------------------------------------------------

/// Load a membership scoped to `org_id` (a membership id from another org 404s).
async fn scoped_membership(
    state: &AppState,
    org_id: Uuid,
    membership_id: Uuid,
) -> Result<Option<Membership>, sqlx::Error> {
    Ok(membership::find_by_id(&state.db, membership_id)
        .await?
        .filter(|m| m.organization_id == org_id))
}

async fn member_edit_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, membership_id)): Path<(Uuid, Uuid)>,
    session: Session,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_manage_members() {
        return console::section_forbidden(&ctx);
    }
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };
    let m = match require_found(
        scoped_membership(&state, org_id, membership_id).await,
        &ctx,
        Section::Members,
        "Member not found.",
    ) {
        Ok(m) => m,
        Err(response) => return response,
    };
    let instruments = load_instruments(&state).await.unwrap_or_default();
    let username = match user::find_by_id(&state.db, m.user_id).await {
        Ok(Some(u)) => u.username,
        _ => m.user_id.to_string(),
    };

    let body = html! {
        h2 { "Member: " (username) }
        form method="post" action=(format!("/admin/orgs/{org_id}/members/{membership_id}")) {
            (layout::csrf_field(&token))
            input type="hidden" name="expected_version" value=(m.updated_at.timestamp_millis());
            label {
                "Role "
                select name="role" {
                    @for role in [Role::Owner, Role::Archivist, Role::Conductor, Role::Musician] {
                        option value=(role.as_str()) selected[role == m.role] { (role.as_str()) }
                    }
                }
            }
            fieldset {
                legend { "Instruments" }
                p class="muted" { "The instruments this member plays (used by coverage checks)." }
                (instrument_multiselect("instrument_ids", &instruments, &m.instrument_ids))
            }
            label {
                input type="checkbox" name="is_principal" value="true" checked[m.is_principal];
                " Principal (section leader)"
            }
            fieldset {
                legend { "Principal instruments" }
                p class="muted" { "Must be a subset of the instruments above; only used when Principal is set." }
                (instrument_multiselect("principal_instrument_ids", &instruments, &m.principal_instrument_ids))
            }
            button type="submit" { "Save member" }
        }
        p { a href=(format!("/admin/orgs/{org_id}/members")) { "← Back to members" } }
    };
    Html(console::console_page(&ctx, Section::Members, body).into_string()).into_response()
}

/// A `<select multiple>` of the instrument vocabulary (grouped by family),
/// pre-selecting `selected`.
fn instrument_multiselect(
    name: &str,
    instruments: &[instrument::Instrument],
    selected: &[Uuid],
) -> Markup {
    // Same family ordering as the voice picker (shared helper), so the two
    // instrument selects group families identically.
    let families = ordered_families(instruments);
    html! {
        select name=(name) multiple size="8" {
            @for family in &families {
                optgroup label=(family) {
                    @for inst in instruments.iter().filter(|i| &i.family == family) {
                        option value=(inst.id) selected[selected.contains(&inst.id)] {
                            (inst.display_name)
                            @if let Some(t) = &inst.transposition { " (" (t) ")" }
                        }
                    }
                }
            }
        }
    }
}

/// Extract all values submitted for `key` (a `<select multiple>` submits the
/// key once per selected option). `serde_urlencoded` collapses repeated keys,
/// so we deserialize the body as ordered pairs instead.
fn multi(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
        .collect()
}

fn field<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn parse_ids(values: &[String]) -> Option<Vec<Uuid>> {
    values.iter().map(|v| Uuid::parse_str(v).ok()).collect()
}

async fn update_member(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, membership_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    // Ordered pairs so `<select multiple>` values aren't collapsed.
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_manage_members() {
        return console::section_forbidden(&ctx);
    }
    let current = match require_found(
        scoped_membership(&state, org_id, membership_id).await,
        &ctx,
        Section::Members,
        "Member not found.",
    ) {
        Ok(m) => m,
        Err(response) => return response,
    };
    // Optimistic concurrency: don't let a stale editor clobber concurrent
    // role/instrument changes.
    if check_if_match(field(&pairs, "expected_version"), current.updated_at).is_err() {
        return console::precondition_page(
            &ctx,
            Section::Members,
            "member",
            &format!("/admin/orgs/{org_id}/members/{membership_id}"),
        );
    }

    let Some(role) = field(&pairs, "role").and_then(Role::parse) else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Please choose a valid role.");
    };
    let Some(instrument_ids) = parse_ids(&multi(&pairs, "instrument_ids")) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Invalid instrument selection.",
        );
    };
    let is_principal = field(&pairs, "is_principal").is_some();
    let Some(principal_instrument_ids) = parse_ids(&multi(&pairs, "principal_instrument_ids"))
    else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Invalid principal-instrument selection.",
        );
    };

    let fields = membership::MembershipFields {
        role,
        instrument_ids: &instrument_ids,
        is_principal,
        principal_instrument_ids: &principal_instrument_ids,
    };
    match membership::update(&state.db, membership_id, fields).await {
        Ok(_) => {
            // Distinguish the security-relevant transitions (CLAUDE.md audit
            // events: "role change, principal-flag change"), matching the /v1
            // membership handler — a plain field edit stays `membership.update`.
            let action = if role != current.role {
                "membership.role_change"
            } else if is_principal != current.is_principal {
                "membership.principal_change"
            } else {
                "membership.update"
            };
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                action,
                "membership",
                Some(membership_id),
                serde_json::json!({
                    "role": role.as_str(),
                    "previousRole": current.role.as_str(),
                    "isPrincipal": is_principal,
                    "previousIsPrincipal": current.is_principal,
                }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/members")).into_response()
        }
        Err(membership::MembershipError::LastOwner) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "This organization must keep at least one owner.",
        ),
        Err(membership::MembershipError::PrincipalNotSubset) => error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Principal instruments must be a subset of the member's instruments.",
        ),
        Err(membership::MembershipError::UnknownInstrument) => error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "One or more instruments no longer exist.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to update membership");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the member.",
            )
        }
    }
}

async fn remove_member(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, membership_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_manage_members() {
        return console::section_forbidden(&ctx);
    }
    if let Err(response) = require_found(
        scoped_membership(&state, org_id, membership_id).await,
        &ctx,
        Section::Members,
        "Member not found.",
    ) {
        return response;
    }
    match membership::delete(&state.db, membership_id).await {
        Ok(true) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "membership.delete",
                "membership",
                Some(membership_id),
                serde_json::json!({}),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/members")).into_response()
        }
        Ok(false) => Redirect::to(&format!("/admin/orgs/{org_id}/members")).into_response(),
        Err(membership::MembershipError::LastOwner) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "This organization must keep at least one owner.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to remove member");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not remove the member.",
            )
        }
    }
}
