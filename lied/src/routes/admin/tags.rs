//! `/admin/orgs/{org}/tags` + tag attach/detach on arrangements (Phase 2,
//! issue #31, slice 3). Per-org tag CRUD and the many-to-many links to
//! arrangements, over the phase-1 `tag` domain.
//!
//! Tags are part of the catalog area (the "Arrangements" console section), so
//! this module reuses the shared screen helpers from
//! [`crate::routes::admin::arrangements`] rather than duplicating them.
//! Authorization mirrors the permission matrix "Manage tags" row =
//! `owner`/`archivist` = [`console::ConsoleCtx::can_edit_arrangements`].

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::html;
use serde::Deserialize;
use tower_sessions::Session;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::audit_log::audit;
use crate::domain::tag;
use crate::listing::SortDirection;
use crate::routes::admin::arrangements::{audit_ctx, csrf_token, error_page, scoped_arrangement};
use crate::routes::admin::console::{self, Section};
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs/:org_id/tags", get(tags_page).post(create_tag))
        .route("/orgs/:org_id/tags/:tag_id", post(update_tag))
        .route("/orgs/:org_id/tags/:tag_id/delete", post(delete_tag))
        .route("/orgs/:org_id/tags/:tag_id/undelete", post(undelete_tag))
        // Attach/detach a tag on a specific arrangement.
        .route("/orgs/:org_id/arrangements/:arr_id/tags", post(attach_tag))
        .route(
            "/orgs/:org_id/arrangements/:arr_id/tags/:tag_id/detach",
            post(detach_tag),
        )
}

#[derive(Deserialize)]
struct TagForm {
    name: Option<String>,
    kind: Option<String>,
}

#[derive(Deserialize)]
struct AttachForm {
    tag_id: Option<String>,
}

fn blank(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Tag management screen
// ---------------------------------------------------------------------------

async fn tags_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    session: Session,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.is_staff() {
        return console::section_forbidden(&ctx);
    }
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };
    let can_edit = ctx.can_edit_arrangements();

    let (tags, _total) = match tag::list_for_org(
        &state.db,
        org_id,
        500,
        0,
        "name",
        SortDirection::Asc,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "failed to list tags");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load tags.",
            );
        }
    };
    let deleted = if can_edit {
        tag::list_deleted_for_org(&state.db, org_id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let body = html! {
        p class="muted" {
            "Tags classify arrangements by theme, mood, era, occasion, style, or "
            "anything else — the free-text \"kind\" groups them."
        }
        @if !can_edit { p class="muted" { "You have read-only access to tags." } }
        table {
            thead { tr { th { "Name" } th { "Kind" } @if can_edit { th {} } } }
            tbody {
                @for t in &tags {
                    tr {
                        @if can_edit {
                            td colspan="3" {
                                form class="inline" method="post" action=(format!("/admin/orgs/{org_id}/tags/{}", t.id)) {
                                    (layout::csrf_field(&token))
                                    input type="text" name="name" value=(t.name);
                                    input type="text" name="kind" value=(t.kind.clone().unwrap_or_default()) placeholder="kind";
                                    button type="submit" { "Save" }
                                }
                                form class="inline" method="post" action=(format!("/admin/orgs/{org_id}/tags/{}/delete", t.id)) {
                                    (layout::csrf_field(&token))
                                    button type="submit" { "Delete" }
                                }
                            }
                        } @else {
                            td { (t.name) }
                            td { (t.kind.clone().unwrap_or_default()) }
                        }
                    }
                }
            }
        }
        @if tags.is_empty() { p class="muted" { "No tags yet." } }

        @if can_edit {
            h2 { "Add a tag" }
            form method="post" action=(format!("/admin/orgs/{org_id}/tags")) {
                (layout::csrf_field(&token))
                label { "Name " input type="text" name="name" required; }
                label { "Kind " input type="text" name="kind" placeholder="theme, mood, occasion, …"; }
                button type="submit" { "Create tag" }
            }

            @if !deleted.is_empty() {
                h2 { "Recently deleted" }
                @for t in &deleted {
                    form class="inline" method="post" action=(format!("/admin/orgs/{org_id}/tags/{}/undelete", t.id)) {
                        (layout::csrf_field(&token))
                        span { (t.name) " " span class="muted" { "(" (t.kind.clone().unwrap_or_default()) ")" } " " }
                        button type="submit" { "Restore" }
                    }
                    br;
                }
            }
        }
        p { a href=(format!("/admin/orgs/{org_id}/arrangements")) { "← Back to arrangements" } }
    };
    Html(console::console_page(&ctx, Section::Arrangements, body).into_string()).into_response()
}

async fn create_tag(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    RequestId(request_id): RequestId,
    Form(form): Form<TagForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    let Some(name) = blank(form.name) else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Tag name must not be empty.");
    };
    let kind = blank(form.kind);
    match tag::create(
        &state.db,
        Uuid::now_v7(),
        org_id,
        &name,
        kind.as_deref(),
        Some(ctx.user().id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "tag.create",
                "tag",
                Some(created.id),
                serde_json::json!({ "name": created.name, "kind": created.kind }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/tags")).into_response()
        }
        Err(tag::TagError::Duplicate) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "A tag with this name and kind already exists in this organization.",
        ),
        Err(tag::TagError::Database(error)) => {
            tracing::error!(%error, "failed to create tag");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create the tag.",
            )
        }
    }
}

async fn update_tag(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, tag_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<TagForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    if !tag_belongs_to_org(&state, org_id, tag_id).await {
        return error_page(&ctx, StatusCode::NOT_FOUND, "Tag not found.");
    }
    let Some(name) = blank(form.name) else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Tag name must not be empty.");
    };
    let kind = blank(form.kind);
    match tag::update(&state.db, tag_id, &name, kind.as_deref()).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "tag.update",
                "tag",
                Some(tag_id),
                serde_json::json!({ "name": name, "kind": kind }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/tags")).into_response()
        }
        Err(tag::TagError::Duplicate) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "A tag with this name and kind already exists in this organization.",
        ),
        Err(tag::TagError::Database(error)) => {
            tracing::error!(%error, "failed to update tag");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the tag.",
            )
        }
    }
}

async fn delete_tag(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, tag_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    if !tag_belongs_to_org(&state, org_id, tag_id).await {
        return error_page(&ctx, StatusCode::NOT_FOUND, "Tag not found.");
    }
    let _ = tag::soft_delete(&state.db, tag_id).await;
    audit(
        &state.db,
        &audit_ctx(&ctx, request_id),
        "tag.soft_delete",
        "tag",
        Some(tag_id),
        serde_json::json!({}),
    )
    .await;
    Redirect::to(&format!("/admin/orgs/{org_id}/tags")).into_response()
}

async fn undelete_tag(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, tag_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    // Scope: the deleted tag must belong to this org.
    let owned = tag::list_deleted_for_org(&state.db, org_id)
        .await
        .map(|ts| ts.iter().any(|t| t.id == tag_id))
        .unwrap_or(false);
    if !owned {
        return error_page(&ctx, StatusCode::NOT_FOUND, "Tag not found.");
    }
    let _ = tag::undelete(&state.db, tag_id).await;
    audit(
        &state.db,
        &audit_ctx(&ctx, request_id),
        "tag.undelete",
        "tag",
        Some(tag_id),
        serde_json::json!({}),
    )
    .await;
    Redirect::to(&format!("/admin/orgs/{org_id}/tags")).into_response()
}

/// Whether a live tag with `tag_id` belongs to `org_id` — the scope check for
/// tag mutations (a tag id from another org must 404, not mutate).
async fn tag_belongs_to_org(state: &AppState, org_id: Uuid, tag_id: Uuid) -> bool {
    matches!(
        tag::find_by_id(&state.db, tag_id).await,
        Ok(Some(t)) if t.organization_id == org_id
    )
}

// ---------------------------------------------------------------------------
// Attach / detach on an arrangement
// ---------------------------------------------------------------------------

async fn attach_tag(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<AttachForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    if scoped_arrangement(&state, &ctx, arr_id, false)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found.");
    }
    let tag_id = match blank(form.tag_id).map(|s| Uuid::parse_str(&s)) {
        Some(Ok(id)) => id,
        _ => return error_page(&ctx, StatusCode::BAD_REQUEST, "Please choose a tag."),
    };
    if !tag_belongs_to_org(&state, org_id, tag_id).await {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "That tag does not exist in this organization.",
        );
    }
    match tag::attach(&state.db, Uuid::now_v7(), arr_id, tag_id).await {
        Ok(()) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "arrangement_tag.create",
                "arrangement_tag",
                Some(arr_id),
                serde_json::json!({ "arrangementId": arr_id, "tagId": tag_id }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/arrangements/{arr_id}")).into_response()
        }
        Err(tag::ArrangementTagError::Duplicate) => {
            // Idempotent from the archivist's view: already attached, so just
            // return to the arrangement rather than error.
            Redirect::to(&format!("/admin/orgs/{org_id}/arrangements/{arr_id}")).into_response()
        }
        Err(tag::ArrangementTagError::Database(error)) => {
            tracing::error!(%error, "failed to attach tag");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not attach the tag.",
            )
        }
    }
}

async fn detach_tag(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, tag_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    if scoped_arrangement(&state, &ctx, arr_id, false)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found.");
    }
    let _ = tag::detach(&state.db, arr_id, tag_id).await;
    audit(
        &state.db,
        &audit_ctx(&ctx, request_id),
        "arrangement_tag.delete",
        "arrangement_tag",
        Some(arr_id),
        serde_json::json!({ "arrangementId": arr_id, "tagId": tag_id }),
    )
    .await;
    Redirect::to(&format!("/admin/orgs/{org_id}/arrangements/{arr_id}")).into_response()
}
