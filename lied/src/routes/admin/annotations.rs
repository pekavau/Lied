//! Global annotations in the console (issue #36): the conductor's notes on an
//! arrangement, for the whole ensemble.
//!
//! Two surfaces:
//! * an **Annotations block on the arrangement screen** ([`section`]) — every
//!   staff role reads it; owner and conductor also get the add form and an
//!   Edit link per note (the archivist's view is read-only, per the matrix);
//! * the **Annotations section** in the nav (`/admin/orgs/{org}/annotations`)
//!   — the most recently changed notes across the org, for the authors who
//!   see that section.
//!
//! Writes go through [`global_annotation`] like the `/v1` routes do, and are
//! gated on [`ConsoleCtx::can_author_global_annotations`]. Deleting is a hard
//! delete; its audit row keeps the content.

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
use crate::domain::global_annotation::{self, GlobalAnnotation, GlobalAnnotationError, WriteError};
use crate::routes::admin::arrangements::{audit_ctx, csrf_token, scoped_arrangement};
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs/:org_id/annotations", get(overview_page))
        .route(
            "/orgs/:org_id/arrangements/:arr_id/annotations",
            post(create_annotation),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/annotations/:id",
            get(edit_page).post(update_annotation),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/annotations/:id/delete",
            post(delete_annotation),
        )
}

#[derive(Deserialize)]
struct AnnotationForm {
    #[serde(rename = "type")]
    annotation_type: Option<String>,
    content: Option<String>,
    expected_version: Option<String>,
}

fn error_page(ctx: &ConsoleCtx, status: StatusCode, message: &str) -> Response {
    console::error_page(ctx, Section::Arrangements, status, message)
}

fn arrangement_url(org_id: Uuid, arr_id: Uuid) -> String {
    format!("/admin/orgs/{org_id}/arrangements/{arr_id}")
}

fn annotation_url(org_id: Uuid, arr_id: Uuid, id: Uuid) -> String {
    format!("{}/annotations/{id}", arrangement_url(org_id, arr_id))
}

/// A human label for a type value.
fn type_label(value: &str) -> &str {
    match value {
        "tempo" => "Tempo",
        "dynamics" => "Dynamics",
        "articulation" => "Articulation",
        "structure" => "Structure (cuts, repeats)",
        "general" => "General",
        other => other,
    }
}

fn type_select(current: Option<&str>) -> Markup {
    html! {
        select name="type" required {
            @for t in global_annotation::TYPES {
                option value=(t) selected[current == Some(*t)] { (type_label(t)) }
            }
        }
    }
}

/// Map a domain refusal to the page the archivist sees.
fn refusal(ctx: &ConsoleCtx, err: GlobalAnnotationError) -> Response {
    match err {
        GlobalAnnotationError::InvalidType => error_page(
            ctx,
            StatusCode::BAD_REQUEST,
            "Choose a type for the annotation.",
        ),
        GlobalAnnotationError::EmptyContent => error_page(
            ctx,
            StatusCode::BAD_REQUEST,
            "The annotation must not be empty.",
        ),
        GlobalAnnotationError::ArrangementGone => error_page(
            ctx,
            StatusCode::NOT_FOUND,
            "The arrangement was removed from the catalog.",
        ),
        GlobalAnnotationError::Database(error) => {
            tracing::error!(%error, "global annotation write failed");
            error_page(
                ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the annotation.",
            )
        }
    }
}

/// Which page of an arrangement's annotations the arrangement screen shows.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnnotationPage {
    pub offset: i64,
    pub limit: i64,
    pub total: i64,
}

/// The Annotations block on the arrangement screen. `annotations` is one page
/// of the arrangement's list (loaded by the caller); `page` says which, so a
/// long list pages instead of leaving notes past the cap with no Edit link
/// anywhere. A removed arrangement shows no add form: its notes are hidden
/// through it and come back when it is restored.
pub(crate) fn section(
    ctx: &ConsoleCtx,
    org_id: Uuid,
    arr_id: Uuid,
    arrangement_removed: bool,
    annotations: &[GlobalAnnotation],
    page: AnnotationPage,
    token: &str,
) -> Markup {
    let can_author = ctx.can_author_global_annotations();
    let base = arrangement_url(org_id, arr_id);
    let shown_to = page.offset + annotations.len() as i64;
    html! {
        h2 { "Annotations" }
        p class="muted" {
            "The conductor's notes for the whole ensemble — tempo decisions, cuts, "
            "dynamic shaping. They are not personal markings on a part."
        }
        @if arrangement_removed {
            p class="muted" {
                "This arrangement was removed; its annotations come back when it is restored."
            }
        } @else {
            @if annotations.is_empty() {
                p class="muted" { "No annotations yet." }
            } @else {
                table {
                    thead { tr { th { "Type" } th { "Annotation" } th { "By" } th { "Changed" } @if can_author { th {} } } }
                    tbody {
                        @for a in annotations {
                            tr {
                                td { (type_label(&a.annotation_type)) }
                                td { (a.content) }
                                td { (a.author_display_name) }
                                td { (layout::utc_time(a.updated_at)) }
                                @if can_author {
                                    td { a href=(annotation_url(org_id, arr_id, a.id)) { "Edit" } }
                                }
                            }
                        }
                    }
                }
                @if page.total > annotations.len() as i64 {
                    p class="muted" {
                        "Annotations " (page.offset + 1) "–" (shown_to) " of " (page.total) ". "
                        @if page.offset > 0 {
                            a href=(format!("{base}?annotations_offset={}", (page.offset - page.limit).max(0))) {
                                "← Earlier"
                            }
                            " "
                        }
                        @if shown_to < page.total {
                            a href=(format!("{base}?annotations_offset={shown_to}")) { "Later →" }
                        }
                    }
                }
            }
            @if can_author {
                h3 { "Add an annotation" }
                form method="post" action=(format!("{}/annotations", arrangement_url(org_id, arr_id))) {
                    (layout::csrf_field(token))
                    label { "Type " (type_select(Some("general"))) }
                    label { "Annotation " textarea name="content" rows="3" required {} }
                    button type="submit" { "Add annotation" }
                }
            } @else {
                p class="muted" { "Only the conductor or an owner can add or change annotations." }
            }
        }
    }
}

/// The version the form was rendered at, if it is still the current one.
/// The write re-checks it atomically; this early answer avoids a pointless
/// statement for a form that is plainly stale.
fn expected_version(form: &AnnotationForm, current: &GlobalAnnotation) -> Option<i64> {
    form.expected_version
        .as_deref()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v == current.updated_at.timestamp_millis())
}

/// CRLF (and lone CR) to LF — what a browser's textarea sends versus what the
/// note was stored with.
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

async fn enter_author(
    state: &AppState,
    auth: Option<AuthSession>,
    org_id: Uuid,
) -> Result<ConsoleCtx, Response> {
    let ctx = console::enter(state, auth, org_id).await?;
    if !ctx.can_author_global_annotations() {
        return Err(console::section_forbidden(&ctx));
    }
    Ok(ctx)
}

/// The annotation, if it belongs to a live arrangement of this org with this
/// id; an id from anywhere else is a 404.
async fn scoped_annotation(
    state: &AppState,
    ctx: &ConsoleCtx,
    org_id: Uuid,
    arr_id: Uuid,
    id: Uuid,
) -> Result<GlobalAnnotation, Response> {
    match global_annotation::find(&state.db, org_id, arr_id, id).await {
        Ok(Some(a)) => Ok(a),
        Ok(None) => Err(error_page(
            ctx,
            StatusCode::NOT_FOUND,
            "Annotation not found.",
        )),
        Err(error) => {
            tracing::error!(%error, "failed to load the annotation");
            Err(error_page(
                ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the annotation.",
            ))
        }
    }
}

async fn create_annotation(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<AnnotationForm>,
) -> Response {
    let ctx = match enter_author(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    match scoped_arrangement(&state, &ctx, arr_id, false).await {
        Ok(Some(_)) => {}
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load the arrangement");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the arrangement.",
            );
        }
    }
    let created = match global_annotation::create(
        &state.db,
        Uuid::now_v7(),
        arr_id,
        ctx.user().id,
        form.annotation_type.as_deref().unwrap_or(""),
        &normalize_newlines(form.content.as_deref().unwrap_or("")),
    )
    .await
    {
        Ok(created) => created,
        Err(err) => return refusal(&ctx, err),
    };
    audit(
        &state.db,
        &audit_ctx(&ctx, request_id),
        "global_annotation.create",
        "global_annotation",
        Some(created.id),
        serde_json::json!({
            "arrangementId": arr_id,
            "type": created.annotation_type,
            "content": created.content,
        }),
    )
    .await;
    Redirect::to(&arrangement_url(org_id, arr_id)).into_response()
}

async fn edit_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, id)): Path<(Uuid, Uuid, Uuid)>,
    session: Session,
) -> Response {
    let ctx = match enter_author(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };
    let a = match scoped_annotation(&state, &ctx, org_id, arr_id, id).await {
        Ok(a) => a,
        Err(response) => return response,
    };
    let version = a.updated_at.timestamp_millis();
    let url = annotation_url(org_id, arr_id, id);
    let body = html! {
        h2 { "Edit annotation" }
        p class="muted" {
            "By " (a.author_display_name) ", last changed "
            (layout::utc_time(a.updated_at)) "."
        }
        form method="post" action=(&url) {
            (layout::csrf_field(&token))
            input type="hidden" name="expected_version" value=(version);
            label { "Type " (type_select(Some(&a.annotation_type))) }
            // The HTML parser drops one newline straight after <textarea>, so
            // emit one: content that itself starts with a newline keeps it.
            label { "Annotation " textarea name="content" rows="4" required { "\n" (a.content) } }
            button type="submit" { "Save changes" }
        }
        form method="post" action=(format!("{url}/delete")) {
            (layout::csrf_field(&token))
            input type="hidden" name="expected_version" value=(version);
            button type="submit" { "Delete annotation" }
            " " span class="muted" { "Deleting cannot be undone." }
        }
        p { a href=(arrangement_url(org_id, arr_id)) { "← Back to the arrangement" } }
    };
    Html(console::console_page(&ctx, Section::Arrangements, body).into_string()).into_response()
}

async fn update_annotation(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<AnnotationForm>,
) -> Response {
    let ctx = match enter_author(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let current = match scoped_annotation(&state, &ctx, org_id, arr_id, id).await {
        Ok(a) => a,
        Err(response) => return response,
    };
    let url = annotation_url(org_id, arr_id, id);
    let Some(expected) = expected_version(&form, &current) else {
        return console::precondition_page(&ctx, Section::Arrangements, "annotation", &url);
    };
    // The edit form always submits both fields, so a missing one is a broken
    // form, not "leave it": treat it like an empty value and let the domain
    // refuse it. Browsers submit textarea line breaks as CRLF; store LF, or
    // saving a note that came in over /v1 would rewrite every line ending
    // nobody touched.
    let annotation_type = form.annotation_type.clone().unwrap_or_default();
    let content = normalize_newlines(form.content.as_deref().unwrap_or(""));
    if annotation_type == current.annotation_type && content == current.content {
        // Nothing changed: no write, no new version, no audit row.
        return Redirect::to(&arrangement_url(org_id, arr_id)).into_response();
    }
    let updated = match global_annotation::update(
        &state.db,
        id,
        expected,
        Some(&annotation_type),
        Some(&content),
    )
    .await
    {
        Ok(updated) => updated,
        Err(WriteError::Stale) => {
            return console::precondition_page(&ctx, Section::Arrangements, "annotation", &url)
        }
        Err(WriteError::NotFound) => {
            return error_page(&ctx, StatusCode::NOT_FOUND, "Annotation not found.")
        }
        Err(WriteError::Invalid(err)) => return refusal(&ctx, err),
    };
    audit(
        &state.db,
        &audit_ctx(&ctx, request_id),
        "global_annotation.update",
        "global_annotation",
        Some(id),
        serde_json::json!({
            "arrangementId": arr_id,
            "before": { "type": current.annotation_type, "content": current.content },
            "after": { "type": updated.annotation_type, "content": updated.content },
        }),
    )
    .await;
    Redirect::to(&arrangement_url(org_id, arr_id)).into_response()
}

async fn delete_annotation(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<AnnotationForm>,
) -> Response {
    let ctx = match enter_author(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let current = match scoped_annotation(&state, &ctx, org_id, arr_id, id).await {
        Ok(a) => a,
        Err(response) => return response,
    };
    let url = annotation_url(org_id, arr_id, id);
    let Some(expected) = expected_version(&form, &current) else {
        return console::precondition_page(&ctx, Section::Arrangements, "annotation", &url);
    };
    // Audit what the write removed: afterwards it is the only copy.
    let removed = match global_annotation::delete(&state.db, id, expected).await {
        Ok(removed) => removed,
        Err(WriteError::Stale) => {
            return console::precondition_page(&ctx, Section::Arrangements, "annotation", &url)
        }
        Err(WriteError::NotFound) => {
            return error_page(&ctx, StatusCode::NOT_FOUND, "Annotation not found.")
        }
        Err(WriteError::Invalid(err)) => return refusal(&ctx, err),
    };
    audit(
        &state.db,
        &audit_ctx(&ctx, request_id),
        "global_annotation.delete",
        "global_annotation",
        Some(id),
        serde_json::json!({
            "hardDelete": true,
            "arrangementId": arr_id,
            "authorId": removed.author_id,
            "type": removed.annotation_type,
            "content": removed.content,
        }),
    )
    .await;
    Redirect::to(&arrangement_url(org_id, arr_id)).into_response()
}

/// The nav's Annotations section: the most recently changed notes across the
/// org, each linked to its arrangement.
async fn overview_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.may_view(Section::Annotations) {
        return console::section_forbidden(&ctx);
    }
    let can_author = ctx.can_author_global_annotations();
    let limit = i64::from(state.config.max_page_size);
    let (rows, total) = match global_annotation::list_for_org(&state.db, org_id, limit).await {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "failed to list the org's annotations");
            return console::error_page(
                &ctx,
                Section::Annotations,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the annotations.",
            );
        }
    };
    let body = html! {
        h2 { "Global annotations" }
        p class="muted" {
            "The most recently changed annotations across the catalog. Add or edit "
            "them on each arrangement's page."
        }
        @if rows.is_empty() {
            p class="muted" { "No annotations yet." }
        } @else {
            table {
                thead { tr { th { "Arrangement" } th { "Type" } th { "Annotation" } th { "By" } th { "Changed" } @if can_author { th {} } } }
                tbody {
                    @for row in &rows {
                        @let a = &row.annotation;
                        tr {
                            td { a href=(arrangement_url(org_id, a.arrangement_id)) { (row.arrangement_title) } }
                            td { (type_label(&a.annotation_type)) }
                            td { (a.content) }
                            td { (a.author_display_name) }
                            td { (layout::utc_time(a.updated_at)) }
                            @if can_author {
                                td { a href=(annotation_url(org_id, a.arrangement_id, a.id)) { "Edit" } }
                            }
                        }
                    }
                }
            }
            @if total > rows.len() as i64 {
                p class="muted" { "Showing the " (rows.len()) " most recent of " (total) "." }
            }
        }
    };
    Html(console::console_page(&ctx, Section::Annotations, body).into_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textarea_line_endings_are_stored_as_lf() {
        assert_eq!(normalize_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(normalize_newlines("plain"), "plain");
    }

    #[test]
    fn every_type_has_a_label() {
        for t in global_annotation::TYPES {
            assert_ne!(type_label(t), *t, "{t} falls through to the raw value");
        }
    }
}
