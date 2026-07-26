//! `/admin/orgs/{org}/arrangements` + `…/works` — the archivist's core
//! back-office data entry (Phase 2, issue #31). HTMX/maud screens over the
//! existing phase-1 `arrangement` and `work` domain functions; no new backend.
//!
//! Authorization comes from the console foundation ([`console::ConsoleCtx`]):
//! any staff role may *view* arrangements; only `owner`/`archivist` may edit
//! (a conductor sees them read-only, per the permission matrix). Works are
//! instance-wide — any member may create one, but editing is restricted to the
//! creator or a system admin ([`work::Work::is_editable_by`]).
//!
//! **Optimistic concurrency:** edit forms carry the arrangement's
//! `updated_at` (ms epoch) in a hidden `expected_version` field; a stale value
//! on submit yields a friendly `412` retry page rather than a silent
//! overwrite (CLAUDE.md HTTP conventions: "HTMX forms carry the ETag in a
//! hidden field").

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use chrono::NaiveDate;
use maud::{html, Markup};
use serde::Deserialize;
use tower_sessions::Session;
use uuid::Uuid;

use crate::auth::csrf;
use crate::auth::extractors::AuthSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::{arrangement, instrument, tag, voice, work};
use crate::listing::SortDirection;
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs/:org_id/arrangements", get(list_page).post(create))
        .route(
            "/orgs/:org_id/arrangements/:arr_id",
            get(detail_page).post(update),
        )
        .route("/orgs/:org_id/arrangements/:arr_id/delete", post(delete))
        .route(
            "/orgs/:org_id/arrangements/:arr_id/undelete",
            post(undelete),
        )
        // Voices belong to an arrangement (Phase 2 issue #31, slice 2).
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices",
            get(voices_page).post(create_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id",
            get(voice_detail_page).post(update_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/delete",
            post(delete_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/undelete",
            post(undelete_voice),
        )
        .route("/orgs/:org_id/works", get(works_page).post(create_work))
        .route("/orgs/:org_id/works/:work_id", post(update_work))
}

/// Difficulty scales offered in the ratings editor (CLAUDE.md: "UI suggests
/// known scales ... via autocomplete"). `abrsm` is special-cased — it must
/// agree with the numeric `difficulty` column.
const RATING_SCALES: &[(&str, &str)] = &[
    ("abrsm", "ABRSM"),
    ("aba", "ABA"),
    ("henle", "Henle"),
    ("rcm", "RCM"),
    ("trinity", "Trinity"),
];

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// Trim a form value and treat the empty string as absent — text inputs always
/// submit (even blank), so `""` means "not provided".
fn blank_to_none(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Render a full-page error inside the arrangements section shell, at `status`.
pub(crate) fn error_page(ctx: &ConsoleCtx, status: StatusCode, message: &str) -> Response {
    let body = html! { p class="error" { (message) } };
    (
        status,
        Html(console::console_page(ctx, Section::Arrangements, body).into_string()),
    )
        .into_response()
}

/// The stale-write retry page (`412`): the row changed since the form loaded.
fn precondition_page(ctx: &ConsoleCtx, arr_id: Uuid) -> Response {
    let body = html! {
        p class="error" {
            "This arrangement was changed by someone else since you opened the "
            "form. Your edit was not saved — reload and try again."
        }
        p {
            a href=(format!("/admin/orgs/{}/arrangements/{}", ctx.org().id, arr_id)) {
                "Reload the arrangement"
            }
        }
    };
    (
        StatusCode::PRECONDITION_FAILED,
        Html(console::console_page(ctx, Section::Arrangements, body).into_string()),
    )
        .into_response()
}

pub(crate) async fn csrf_token(ctx: &ConsoleCtx, session: &Session) -> Result<String, Response> {
    csrf::ensure_token(session).await.map_err(|error| {
        tracing::error!(%error, "failed to establish CSRF token");
        error_page(
            ctx,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not prepare the form. Please retry.",
        )
    })
}

pub(crate) fn audit_ctx(ctx: &ConsoleCtx, request_id: Uuid) -> AuditContext {
    AuditContext {
        actor_user_id: Some(ctx.user().id),
        org_id: Some(ctx.org().id),
        request_id: Some(request_id),
    }
}

/// Fetch an arrangement scoped to `ctx`'s org, or `None` if it doesn't exist
/// or belongs to another org (cross-org access is a 404, never a leak).
pub(crate) async fn scoped_arrangement(
    state: &AppState,
    ctx: &ConsoleCtx,
    arr_id: Uuid,
    including_deleted: bool,
) -> Result<Option<arrangement::Arrangement>, sqlx::Error> {
    let found = if including_deleted {
        arrangement::find_by_id_including_deleted(&state.db, arr_id).await?
    } else {
        arrangement::find_by_id(&state.db, arr_id).await?
    };
    Ok(found.filter(|a| a.organization_id == ctx.org().id))
}

// ---------------------------------------------------------------------------
// Form parsing / validation
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ArrangementForm {
    title: Option<String>,
    work_id: Option<String>,
    instrumentation: Option<String>,
    arranger: Option<String>,
    publisher: Option<String>,
    purchase_date: Option<String>,
    license_notes: Option<String>,
    copy_count_allowed: Option<String>,
    status: Option<String>,
    duration_seconds: Option<String>,
    difficulty: Option<String>,
    rating_abrsm: Option<String>,
    rating_aba: Option<String>,
    rating_henle: Option<String>,
    rating_rcm: Option<String>,
    rating_trinity: Option<String>,
    difficulty_notes: Option<String>,
    expected_version: Option<String>,
}

/// Owned, validated arrangement fields, ready to borrow into
/// [`arrangement::ArrangementFields`].
struct Parsed {
    title: String,
    work_id: Option<Uuid>,
    instrumentation: Option<String>,
    arranger: Option<String>,
    publisher: Option<String>,
    purchase_date: Option<NaiveDate>,
    license_notes: Option<String>,
    copy_count_allowed: Option<i32>,
    status: String,
    duration_seconds: Option<i32>,
    difficulty: Option<i16>,
    difficulty_ratings: Option<serde_json::Value>,
    difficulty_notes: Option<String>,
}

impl ArrangementForm {
    fn rating(&self, key: &str) -> Option<String> {
        let raw = match key {
            "abrsm" => &self.rating_abrsm,
            "aba" => &self.rating_aba,
            "henle" => &self.rating_henle,
            "rcm" => &self.rating_rcm,
            "trinity" => &self.rating_trinity,
            _ => &None,
        };
        blank_to_none(raw.clone())
    }

    /// Validate and coerce the raw form into [`Parsed`], or return a
    /// human-readable message for the first problem found.
    fn parse(&self) -> Result<Parsed, String> {
        let title = blank_to_none(self.title.clone()).ok_or("Title must not be empty.")?;

        let work_id = match blank_to_none(self.work_id.clone()) {
            Some(raw) => Some(Uuid::parse_str(&raw).map_err(|_| "Invalid work selection.")?),
            None => None,
        };

        let purchase_date = match blank_to_none(self.purchase_date.clone()) {
            Some(raw) => Some(
                NaiveDate::parse_from_str(&raw, "%Y-%m-%d")
                    .map_err(|_| "Purchase date must be a valid date (YYYY-MM-DD).")?,
            ),
            None => None,
        };

        let copy_count_allowed = match blank_to_none(self.copy_count_allowed.clone()) {
            Some(raw) => {
                let n: i32 = raw
                    .parse()
                    .map_err(|_| "Copy count must be a whole number.")?;
                if n < 0 {
                    return Err("Copy count must not be negative.".to_string());
                }
                Some(n)
            }
            None => None,
        };

        let status = blank_to_none(self.status.clone()).unwrap_or_else(|| "active".to_string());
        if status != "active" && status != "archived" {
            return Err("Status must be either active or archived.".to_string());
        }

        let duration_seconds = match blank_to_none(self.duration_seconds.clone()) {
            Some(raw) => {
                let n: i32 = raw
                    .parse()
                    .map_err(|_| "Duration must be a whole number of seconds.")?;
                if n < 0 {
                    return Err("Duration must not be negative.".to_string());
                }
                Some(n)
            }
            None => None,
        };

        let difficulty = match blank_to_none(self.difficulty.clone()) {
            Some(raw) => {
                let n: i16 = raw
                    .parse()
                    .map_err(|_| "Difficulty must be a whole number 1–8.")?;
                if !(1..=8).contains(&n) {
                    return Err("Difficulty must be between 1 and 8 (ABRSM).".to_string());
                }
                Some(n)
            }
            None => None,
        };

        // Assemble the ratings map from the per-scale inputs.
        let mut ratings = serde_json::Map::new();
        for (key, _label) in RATING_SCALES {
            if let Some(value) = self.rating(key) {
                ratings.insert((*key).to_string(), serde_json::Value::String(value));
            }
        }
        // ABRSM key, if present, must agree with the numeric difficulty
        // (rounded) — CLAUDE.md difficulty_ratings rule.
        if let (Some(diff), Some(serde_json::Value::String(abrsm))) =
            (difficulty, ratings.get("abrsm"))
        {
            let abrsm_num: f64 = abrsm
                .parse()
                .map_err(|_| "ABRSM rating must be a number.")?;
            if abrsm_num.round() as i16 != diff {
                return Err(format!(
                    "The ABRSM rating ({abrsm}) must agree with the difficulty ({diff})."
                ));
            }
        }
        let difficulty_ratings = if ratings.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(ratings))
        };

        Ok(Parsed {
            title,
            work_id,
            instrumentation: blank_to_none(self.instrumentation.clone()),
            arranger: blank_to_none(self.arranger.clone()),
            publisher: blank_to_none(self.publisher.clone()),
            purchase_date,
            license_notes: blank_to_none(self.license_notes.clone()),
            copy_count_allowed,
            status,
            duration_seconds,
            difficulty,
            difficulty_ratings,
            difficulty_notes: blank_to_none(self.difficulty_notes.clone()),
        })
    }
}

impl Parsed {
    fn as_fields(&self) -> arrangement::ArrangementFields<'_> {
        arrangement::ArrangementFields {
            title: &self.title,
            work_id: self.work_id,
            instrumentation: self.instrumentation.as_deref(),
            arranger: self.arranger.as_deref(),
            publisher: self.publisher.as_deref(),
            purchase_date: self.purchase_date,
            license_notes: self.license_notes.as_deref(),
            copy_count_allowed: self.copy_count_allowed,
            status: &self.status,
            duration_seconds: self.duration_seconds,
            difficulty: self.difficulty,
            difficulty_ratings: self.difficulty_ratings.clone(),
            difficulty_notes: self.difficulty_notes.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------
// Arrangements: list + create
// ---------------------------------------------------------------------------

async fn list_page(
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

    let (arrangements, _total) = match arrangement::list_for_org(
        &state.db,
        org_id,
        200,
        0,
        "title",
        SortDirection::Asc,
        None,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "failed to list arrangements");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load arrangements.",
            );
        }
    };

    let can_edit = ctx.can_edit_arrangements();
    let works = load_works(&state, &ctx).await.unwrap_or_default();

    let body = html! {
        @if !can_edit {
            p class="muted" { "You have read-only access to arrangements." }
        }
        table {
            thead { tr { th { "Title" } th { "Status" } th { "Difficulty" } th {} } }
            tbody {
                @for a in &arrangements {
                    tr {
                        td { (a.title) }
                        td { (a.status) }
                        td { @if let Some(d) = a.difficulty { (d) } @else { "—" } }
                        td {
                            a href=(format!("/admin/orgs/{org_id}/arrangements/{}", a.id)) {
                                @if can_edit { "Edit" } @else { "View" }
                            }
                        }
                    }
                }
            }
        }
        @if arrangements.is_empty() {
            p class="muted" { "No arrangements yet." }
        }
        p {
            a href=(format!("/admin/orgs/{org_id}/works")) { "Manage works →" }
            " · "
            a href=(format!("/admin/orgs/{org_id}/tags")) { "Manage tags →" }
        }

        @if can_edit {
            h2 { "Add an arrangement" }
            form method="post" action=(format!("/admin/orgs/{org_id}/arrangements")) {
                (layout::csrf_field(&token))
                (arrangement_form_fields(None, &works))
                button type="submit" { "Create arrangement" }
            }
        }
    };
    Html(console::console_page(&ctx, Section::Arrangements, body).into_string()).into_response()
}

async fn create(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    RequestId(request_id): RequestId,
    Form(form): Form<ArrangementForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    let parsed = match form.parse() {
        Ok(parsed) => parsed,
        Err(message) => return error_page(&ctx, StatusCode::BAD_REQUEST, &message),
    };

    let id = Uuid::now_v7();
    let slug = arrangement::slugify(&parsed.title);
    match arrangement::create(
        &state.db,
        id,
        org_id,
        &slug,
        parsed.as_fields(),
        Some(ctx.user().id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "arrangement.create",
                "arrangement",
                Some(created.id),
                serde_json::json!({ "title": created.title, "slug": created.slug }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/arrangements/{id}")).into_response()
        }
        Err(arrangement::ArrangementError::DuplicateSlug) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "An arrangement with a slug derived from this title already exists in this organization.",
        ),
        Err(arrangement::ArrangementError::UnknownWork) => {
            error_page(&ctx, StatusCode::BAD_REQUEST, "The selected work no longer exists.")
        }
        Err(arrangement::ArrangementError::Database(error)) => {
            tracing::error!(%error, "failed to create arrangement");
            error_page(&ctx, StatusCode::INTERNAL_SERVER_ERROR, "Could not create the arrangement.")
        }
    }
}

// ---------------------------------------------------------------------------
// Arrangements: detail / update / delete / undelete
// ---------------------------------------------------------------------------

async fn detail_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
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

    let a = match scoped_arrangement(&state, &ctx, arr_id, true).await {
        Ok(Some(a)) => a,
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load arrangement");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the arrangement.",
            );
        }
    };

    let can_edit = ctx.can_edit_arrangements();
    let works = load_works(&state, &ctx).await.unwrap_or_default();
    let attached_tags = tag::list_tags_for_arrangement(&state.db, arr_id)
        .await
        .unwrap_or_default();
    let (all_tags, _) =
        tag::list_for_org(&state.db, org_id, 500, 0, "name", SortDirection::Asc, None)
            .await
            .unwrap_or_default();
    let version = a.updated_at.timestamp_millis();
    let deleted = a.deleted_at.is_some();

    let body = html! {
        p { "Slug: " code { (a.slug) } " · Status: " (a.status)
            @if deleted { " · " span class="error" { "deleted" } } }

        @if deleted && can_edit {
            form method="post" action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/undelete")) {
                (layout::csrf_field(&token))
                button type="submit" { "Restore this arrangement" }
            }
        } @else if can_edit {
            form method="post" action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}")) {
                (layout::csrf_field(&token))
                input type="hidden" name="expected_version" value=(version);
                (arrangement_form_fields(Some(&a), &works))
                button type="submit" { "Save changes" }
            }
            form method="post" action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/delete")) {
                (layout::csrf_field(&token))
                button type="submit" { "Delete arrangement" }
            }
        } @else {
            (arrangement_readonly(&a, &works))
        }

        h2 { "Tags" }
        @if attached_tags.is_empty() {
            p class="muted" { "No tags attached." }
        } @else {
            p {
                @for t in &attached_tags {
                    span {
                        (t.name)
                        @if let Some(k) = &t.kind { " " span class="muted" { "(" (k) ")" } }
                        @if can_edit {
                            " "
                            form class="inline" method="post"
                                action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/tags/{}/detach", t.id)) {
                                (layout::csrf_field(&token))
                                button type="submit" title="Detach" { "×" }
                            }
                        }
                    }
                    "  "
                }
            }
        }
        @if can_edit {
            @let unattached: Vec<&tag::Tag> = all_tags
                .iter()
                .filter(|t| !attached_tags.iter().any(|a| a.id == t.id))
                .collect();
            @if unattached.is_empty() {
                p class="muted" { "No more tags to attach — " a href=(format!("/admin/orgs/{org_id}/tags")) { "create some" } "." }
            } @else {
                form class="inline" method="post" action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/tags")) {
                    (layout::csrf_field(&token))
                    select name="tag_id" required {
                        option value="" { "— choose a tag —" }
                        @for t in &unattached {
                            option value=(t.id) {
                                (t.name) @if let Some(k) = &t.kind { " (" (k) ")" }
                            }
                        }
                    }
                    button type="submit" { "Attach tag" }
                }
            }
        }

        p {
            a href=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/voices")) { "Manage voices →" }
            " · "
            a href=(format!("/admin/orgs/{org_id}/tags")) { "Manage tags →" }
            " · "
            a href=(format!("/admin/orgs/{org_id}/arrangements")) { "← Back to arrangements" }
        }
    };
    Html(console::console_page(&ctx, Section::Arrangements, body).into_string()).into_response()
}

async fn update(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<ArrangementForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }

    let current = match scoped_arrangement(&state, &ctx, arr_id, false).await {
        Ok(Some(a)) => a,
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load arrangement for update");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the arrangement.",
            );
        }
    };

    // Optimistic concurrency: reject a stale edit rather than clobber a newer one.
    let expected: Option<i64> = form
        .expected_version
        .as_deref()
        .and_then(|v| v.parse().ok());
    if expected != Some(current.updated_at.timestamp_millis()) {
        return precondition_page(&ctx, arr_id);
    }

    let parsed = match form.parse() {
        Ok(parsed) => parsed,
        Err(message) => return error_page(&ctx, StatusCode::BAD_REQUEST, &message),
    };

    match arrangement::update(&state.db, arr_id, parsed.as_fields()).await {
        Ok(Some(updated)) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "arrangement.update",
                "arrangement",
                Some(updated.id),
                serde_json::json!({ "title": updated.title, "status": updated.status }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/arrangements/{arr_id}")).into_response()
        }
        Ok(None) => error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        Err(arrangement::ArrangementError::UnknownWork) => error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "The selected work no longer exists.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to update arrangement");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the arrangement.",
            )
        }
    }
}

async fn delete(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    match scoped_arrangement(&state, &ctx, arr_id, false).await {
        Ok(Some(_)) => {}
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load arrangement for delete");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not delete the arrangement.",
            );
        }
    }
    match arrangement::soft_delete(&state.db, arr_id).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "arrangement.soft_delete",
                "arrangement",
                Some(arr_id),
                serde_json::json!({}),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/arrangements/{arr_id}")).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed to soft-delete arrangement");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not delete the arrangement.",
            )
        }
    }
}

async fn undelete(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    // Scope check against the (possibly deleted) row.
    match scoped_arrangement(&state, &ctx, arr_id, true).await {
        Ok(Some(_)) => {}
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load arrangement for undelete");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not restore the arrangement.",
            );
        }
    }
    match arrangement::undelete(&state.db, arr_id).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "arrangement.undelete",
                "arrangement",
                Some(arr_id),
                serde_json::json!({}),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/arrangements/{arr_id}")).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed to undelete arrangement");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not restore the arrangement.",
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Works (instance-wide; create = any member, edit = creator or system admin)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct WorkForm {
    title: Option<String>,
    composer: Option<String>,
}

async fn load_works(state: &AppState, ctx: &ConsoleCtx) -> Result<Vec<work::Work>, ()> {
    match work::list(&state.db, 200, 0, "title", SortDirection::Asc, None, None).await {
        Ok((works, _total)) => Ok(works),
        Err(error) => {
            tracing::error!(%error, "failed to list works");
            let _ = ctx;
            Err(())
        }
    }
}

async fn works_page(
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
    let works = match load_works(&state, &ctx).await {
        Ok(works) => works,
        Err(()) => {
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load works.",
            )
        }
    };

    let user_id = ctx.user().id;
    let is_admin = ctx.user().is_system_admin;

    let body = html! {
        p class="muted" {
            "Works are the abstract pieces arrangements belong to, shared across "
            "organizations. Anyone may add one; only its creator (or a system "
            "admin) may edit it."
        }
        @for w in &works {
            fieldset {
                @if w.is_editable_by(user_id, is_admin) {
                    form method="post" action=(format!("/admin/orgs/{org_id}/works/{}", w.id)) {
                        (layout::csrf_field(&token))
                        label { "Title " input type="text" name="title" value=(w.title); }
                        label { "Composer " input type="text" name="composer" value=(w.composer.clone().unwrap_or_default()); }
                        button type="submit" { "Save" }
                    }
                } @else {
                    p { strong { (w.title) } @if let Some(c) = &w.composer { " — " (c) } }
                }
            }
        }
        @if works.is_empty() { p class="muted" { "No works yet." } }

        h2 { "Add a work" }
        form method="post" action=(format!("/admin/orgs/{org_id}/works")) {
            (layout::csrf_field(&token))
            label { "Title " input type="text" name="title" required; }
            label { "Composer " input type="text" name="composer"; }
            button type="submit" { "Create work" }
        }
        p { a href=(format!("/admin/orgs/{org_id}/arrangements")) { "← Back to arrangements" } }
    };
    Html(console::console_page(&ctx, Section::Arrangements, body).into_string()).into_response()
}

async fn create_work(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    RequestId(request_id): RequestId,
    Form(form): Form<WorkForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    // Any member may create a work; console access already requires staff.
    if !ctx.is_staff() {
        return console::section_forbidden(&ctx);
    }
    let Some(title) = blank_to_none(form.title) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Work title must not be empty.",
        );
    };
    let composer = blank_to_none(form.composer);
    match work::create(
        &state.db,
        Uuid::now_v7(),
        &title,
        composer.as_deref(),
        Some(ctx.user().id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "work.create",
                "work",
                Some(created.id),
                serde_json::json!({ "title": created.title }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/works")).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed to create work");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create the work.",
            )
        }
    }
}

async fn update_work(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, work_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<WorkForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.is_staff() {
        return console::section_forbidden(&ctx);
    }
    let existing = match work::find_by_id(&state.db, work_id).await {
        Ok(Some(w)) => w,
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Work not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load work");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the work.",
            );
        }
    };
    // Work edit rule: creator or system admin only.
    if !existing.is_editable_by(ctx.user().id, ctx.user().is_system_admin) {
        return error_page(
            &ctx,
            StatusCode::FORBIDDEN,
            "Only the work's creator or a system admin may edit it.",
        );
    }
    let Some(title) = blank_to_none(form.title) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Work title must not be empty.",
        );
    };
    let composer = blank_to_none(form.composer);
    match work::update(&state.db, work_id, &title, composer.as_deref()).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "work.update",
                "work",
                Some(work_id),
                serde_json::json!({ "title": title }),
            )
            .await;
            Redirect::to(&format!("/admin/orgs/{org_id}/works")).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed to update work");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the work.",
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Form field rendering
// ---------------------------------------------------------------------------

/// The editable field set shared by the create and edit forms. `existing`
/// pre-fills values on the edit form; `works` populates the work picker.
fn arrangement_form_fields(
    existing: Option<&arrangement::Arrangement>,
    works: &[work::Work],
) -> Markup {
    let val = |f: fn(&arrangement::Arrangement) -> Option<String>| -> String {
        existing.and_then(f).unwrap_or_default()
    };
    let ratings = existing.and_then(|a| a.difficulty_ratings.as_ref());
    let rating_val = |key: &str| -> String {
        ratings
            .and_then(|r| r.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let selected_work = existing.and_then(|a| a.work_id);
    let selected_status = existing
        .map(|a| a.status.clone())
        .unwrap_or_else(|| "active".to_string());

    html! {
        label { "Title " input type="text" name="title" required
            value=(existing.map(|a| a.title.clone()).unwrap_or_default()); }
        label { "Work "
            select name="work_id" {
                option value="" { "— none —" }
                @for w in works {
                    option value=(w.id) selected[selected_work == Some(w.id)] { (w.title) }
                }
            }
        }
        label { "Instrumentation " input type="text" name="instrumentation"
            value=(val(|a| a.instrumentation.clone())); }
        label { "Arranger " input type="text" name="arranger" value=(val(|a| a.arranger.clone())); }
        label { "Publisher " input type="text" name="publisher" value=(val(|a| a.publisher.clone())); }
        label { "Purchase date " input type="date" name="purchase_date"
            value=(existing.and_then(|a| a.purchase_date).map(|d| d.to_string()).unwrap_or_default()); }
        label { "License notes " input type="text" name="license_notes" value=(val(|a| a.license_notes.clone())); }
        label { "Copies allowed " input type="number" name="copy_count_allowed" min="0"
            value=(existing.and_then(|a| a.copy_count_allowed).map(|n| n.to_string()).unwrap_or_default()); }
        label { "Status "
            select name="status" {
                option value="active" selected[selected_status == "active"] { "active" }
                option value="archived" selected[selected_status == "archived"] { "archived" }
            }
        }
        label { "Duration (seconds) " input type="number" name="duration_seconds" min="0"
            value=(existing.and_then(|a| a.duration_seconds).map(|n| n.to_string()).unwrap_or_default()); }
        label { "Difficulty (1–8, ABRSM) " input type="number" name="difficulty" min="1" max="8"
            value=(existing.and_then(|a| a.difficulty).map(|n| n.to_string()).unwrap_or_default()); }
        fieldset {
            legend { "Difficulty ratings (other scales)" }
            @for (key, label) in RATING_SCALES {
                label { (label) " " input type="text" name=(format!("rating_{key}")) value=(rating_val(key)); }
            }
        }
        label { "Difficulty notes " input type="text" name="difficulty_notes" value=(val(|a| a.difficulty_notes.clone())); }
    }
}

/// Read-only rendering of an arrangement for a conductor.
fn arrangement_readonly(a: &arrangement::Arrangement, works: &[work::Work]) -> Markup {
    let work_title = a
        .work_id
        .and_then(|id| works.iter().find(|w| w.id == id))
        .map(|w| w.title.clone());
    html! {
        table {
            tr { th { "Title" } td { (a.title) } }
            @if let Some(t) = work_title { tr { th { "Work" } td { (t) } } }
            @if let Some(v) = &a.instrumentation { tr { th { "Instrumentation" } td { (v) } } }
            @if let Some(v) = &a.arranger { tr { th { "Arranger" } td { (v) } } }
            @if let Some(v) = &a.publisher { tr { th { "Publisher" } td { (v) } } }
            @if let Some(d) = a.difficulty { tr { th { "Difficulty" } td { (d) } } }
            @if let Some(v) = &a.difficulty_notes { tr { th { "Difficulty notes" } td { (v) } } }
        }
    }
}

// ===========================================================================
// Voices (issue #31, slice 2) — individual instrument parts within an
// arrangement, with the instrument picker drawn from the Instrument vocabulary
// grouped by family.
// ===========================================================================

/// Instrument families in a sensible display order for the picker's optgroups;
/// any family not listed falls to the end under its own heading.
const FAMILY_ORDER: &[&str] = &[
    "woodwind",
    "brass",
    "strings",
    "percussion",
    "keyboard",
    "voice",
    "other",
];

#[derive(Deserialize)]
struct VoiceForm {
    name: Option<String>,
    instrument_id: Option<String>,
    expected_version: Option<String>,
}

/// Load the whole instrument vocabulary (~177 rows) for the picker.
async fn load_instruments(state: &AppState) -> Result<Vec<instrument::Instrument>, sqlx::Error> {
    let (instruments, _total) = instrument::list(&state.db, 1000, 0).await?;
    Ok(instruments)
}

/// A `<select name="instrument_id">` with the vocabulary grouped into
/// `<optgroup>`s by family, `selected` pre-selecting the current instrument.
fn instrument_picker(instruments: &[instrument::Instrument], selected: Option<Uuid>) -> Markup {
    // Families present, ordered by FAMILY_ORDER then any extras alphabetically.
    let mut families: Vec<&str> = instruments.iter().map(|i| i.family.as_str()).collect();
    families.sort_unstable();
    families.dedup();
    families.sort_by_key(|f| {
        FAMILY_ORDER
            .iter()
            .position(|o| o == f)
            .unwrap_or(FAMILY_ORDER.len())
    });

    html! {
        select name="instrument_id" required {
            @for family in &families {
                optgroup label=(family) {
                    @for inst in instruments.iter().filter(|i| &i.family == family) {
                        option value=(inst.id) selected[selected == Some(inst.id)] {
                            (inst.display_name)
                            @if let Some(t) = &inst.transposition { " (" (t) ")" }
                        }
                    }
                }
            }
        }
    }
}

/// Render the instrument's display name for a voice row (falls back to the raw
/// id if the instrument somehow isn't in the loaded set).
fn instrument_name(instruments: &[instrument::Instrument], id: Uuid) -> String {
    instruments
        .iter()
        .find(|i| i.id == id)
        .map(|i| i.display_name.clone())
        .unwrap_or_else(|| id.to_string())
}

async fn voices_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
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

    let a = match scoped_arrangement(&state, &ctx, arr_id, false).await {
        Ok(Some(a)) => a,
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load arrangement for voices");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load voices.",
            );
        }
    };

    let (voices, _total) = match voice::list_for_arrangement(
        &state.db,
        arr_id,
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
            tracing::error!(%error, "failed to list voices");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load voices.",
            );
        }
    };
    let instruments = load_instruments(&state).await.unwrap_or_default();
    let can_edit = ctx.can_edit_arrangements();

    let body = html! {
        h2 { "Voices — " (a.title) }
        @if !can_edit { p class="muted" { "You have read-only access to voices." } }
        table {
            thead { tr { th { "Voice" } th { "Instrument" } @if can_edit { th {} } } }
            tbody {
                @for v in &voices {
                    tr {
                        td { (v.name) }
                        td { (instrument_name(&instruments, v.instrument_id)) }
                        @if can_edit {
                            td {
                                a href=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/voices/{}", v.id)) { "Edit" }
                            }
                        }
                    }
                }
            }
        }
        @if voices.is_empty() { p class="muted" { "No voices yet." } }

        @if can_edit {
            h2 { "Add a voice" }
            form method="post" action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/voices")) {
                (layout::csrf_field(&token))
                label { "Name " input type="text" name="name" required; }
                label { "Instrument " (instrument_picker(&instruments, None)) }
                button type="submit" { "Add voice" }
            }
        }
        p { a href=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}")) { "← Back to arrangement" } }
    };
    Html(console::console_page(&ctx, Section::Arrangements, body).into_string()).into_response()
}

/// Load a voice scoped to `arr_id` (which must belong to `ctx`'s org), or
/// `None` for any mismatch — cross-org / cross-arrangement access is a 404.
async fn scoped_voice(
    state: &AppState,
    ctx: &ConsoleCtx,
    arr_id: Uuid,
    voice_id: Uuid,
    including_deleted: bool,
) -> Result<Option<voice::Voice>, sqlx::Error> {
    // The parent arrangement must belong to this org.
    if scoped_arrangement(state, ctx, arr_id, true)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let found = if including_deleted {
        voice::find_by_id_including_deleted(&state.db, voice_id).await?
    } else {
        voice::find_by_id(&state.db, voice_id).await?
    };
    Ok(found.filter(|v| v.arrangement_id == arr_id))
}

async fn create_voice(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<VoiceForm>,
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
    let Some(name) = blank_to_none(form.name) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Voice name must not be empty.",
        );
    };
    let instrument_id = match blank_to_none(form.instrument_id).map(|s| Uuid::parse_str(&s)) {
        Some(Ok(id)) => id,
        _ => {
            return error_page(
                &ctx,
                StatusCode::BAD_REQUEST,
                "Please choose an instrument.",
            )
        }
    };
    let slug = voice::slugify(&name);
    match voice::create(
        &state.db,
        Uuid::now_v7(),
        arr_id,
        &name,
        &slug,
        instrument_id,
        Some(ctx.user().id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "voice.create",
                "voice",
                Some(created.id),
                serde_json::json!({ "name": created.name, "slug": created.slug }),
            )
            .await;
            Redirect::to(&format!(
                "/admin/orgs/{org_id}/arrangements/{arr_id}/voices"
            ))
            .into_response()
        }
        Err(voice::VoiceError::DuplicateSlug) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "A voice with a slug derived from this name already exists in this arrangement.",
        ),
        Err(voice::VoiceError::UnknownInstrument) => error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "That instrument no longer exists.",
        ),
        Err(voice::VoiceError::Database(error)) => {
            tracing::error!(%error, "failed to create voice");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create the voice.",
            )
        }
    }
}

async fn voice_detail_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id)): Path<(Uuid, Uuid, Uuid)>,
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
    let v = match scoped_voice(&state, &ctx, arr_id, voice_id, true).await {
        Ok(Some(v)) => v,
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Voice not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load voice");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the voice.",
            );
        }
    };
    let instruments = load_instruments(&state).await.unwrap_or_default();
    let can_edit = ctx.can_edit_arrangements();
    let version = v.updated_at.timestamp_millis();
    let deleted = v.deleted_at.is_some();

    let body = html! {
        h2 { "Voice: " (v.name) }
        p { "Slug: " code { (v.slug) } @if deleted { " · " span class="error" { "deleted" } } }
        @if deleted && can_edit {
            form method="post" action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}/undelete")) {
                (layout::csrf_field(&token))
                button type="submit" { "Restore this voice" }
            }
        } @else if can_edit {
            form method="post" action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}")) {
                (layout::csrf_field(&token))
                input type="hidden" name="expected_version" value=(version);
                label { "Name " input type="text" name="name" value=(v.name) required; }
                label { "Instrument " (instrument_picker(&instruments, Some(v.instrument_id))) }
                button type="submit" { "Save changes" }
            }
            form method="post" action=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}/delete")) {
                (layout::csrf_field(&token))
                button type="submit" { "Delete voice" }
            }
        } @else {
            p { "Instrument: " (instrument_name(&instruments, v.instrument_id)) }
        }
        p { a href=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/voices")) { "← Back to voices" } }
    };
    Html(console::console_page(&ctx, Section::Arrangements, body).into_string()).into_response()
}

async fn update_voice(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<VoiceForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    let current = match scoped_voice(&state, &ctx, arr_id, voice_id, false).await {
        Ok(Some(v)) => v,
        Ok(None) => return error_page(&ctx, StatusCode::NOT_FOUND, "Voice not found."),
        Err(error) => {
            tracing::error!(%error, "failed to load voice for update");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the voice.",
            );
        }
    };
    let expected: Option<i64> = form
        .expected_version
        .as_deref()
        .and_then(|v| v.parse().ok());
    if expected != Some(current.updated_at.timestamp_millis()) {
        return precondition_page(&ctx, arr_id);
    }
    let Some(name) = blank_to_none(form.name) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Voice name must not be empty.",
        );
    };
    let instrument_id = match blank_to_none(form.instrument_id).map(|s| Uuid::parse_str(&s)) {
        Some(Ok(id)) => id,
        _ => {
            return error_page(
                &ctx,
                StatusCode::BAD_REQUEST,
                "Please choose an instrument.",
            )
        }
    };
    match voice::update(&state.db, voice_id, &name, instrument_id).await {
        Ok(Some(updated)) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "voice.update",
                "voice",
                Some(updated.id),
                serde_json::json!({ "name": updated.name }),
            )
            .await;
            Redirect::to(&format!(
                "/admin/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}"
            ))
            .into_response()
        }
        Ok(None) => error_page(&ctx, StatusCode::NOT_FOUND, "Voice not found."),
        Err(voice::VoiceError::UnknownInstrument) => error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "That instrument no longer exists.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to update voice");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the voice.",
            )
        }
    }
}

async fn delete_voice(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    if scoped_voice(&state, &ctx, arr_id, voice_id, false)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return error_page(&ctx, StatusCode::NOT_FOUND, "Voice not found.");
    }
    match voice::soft_delete(&state.db, voice_id).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "voice.soft_delete",
                "voice",
                Some(voice_id),
                serde_json::json!({}),
            )
            .await;
            Redirect::to(&format!(
                "/admin/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}"
            ))
            .into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed to soft-delete voice");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not delete the voice.",
            )
        }
    }
}

async fn undelete_voice(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    if scoped_voice(&state, &ctx, arr_id, voice_id, true)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return error_page(&ctx, StatusCode::NOT_FOUND, "Voice not found.");
    }
    match voice::undelete(&state.db, voice_id).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "voice.undelete",
                "voice",
                Some(voice_id),
                serde_json::json!({}),
            )
            .await;
            Redirect::to(&format!(
                "/admin/orgs/{org_id}/arrangements/{arr_id}/voices/{voice_id}"
            ))
            .into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed to undelete voice");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not restore the voice.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_title() -> ArrangementForm {
        ArrangementForm {
            title: Some("Bolero".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn parse_requires_a_non_blank_title() {
        let f = ArrangementForm {
            title: Some("   ".to_string()),
            ..Default::default()
        };
        assert!(f.parse().is_err());
    }

    #[test]
    fn status_defaults_to_active_and_rejects_unknown() {
        assert_eq!(with_title().parse().unwrap().status, "active");
        let f = ArrangementForm {
            status: Some("retired".to_string()),
            ..with_title()
        };
        assert!(f.parse().is_err());
    }

    #[test]
    fn difficulty_must_be_1_to_8() {
        for (raw, ok) in [
            ("1", true),
            ("8", true),
            ("0", false),
            ("9", false),
            ("x", false),
        ] {
            let f = ArrangementForm {
                difficulty: Some(raw.to_string()),
                ..with_title()
            };
            assert_eq!(f.parse().is_ok(), ok, "difficulty {raw}");
        }
    }

    #[test]
    fn abrsm_rating_must_agree_with_difficulty() {
        let agree = ArrangementForm {
            difficulty: Some("6".to_string()),
            rating_abrsm: Some("6".to_string()),
            ..with_title()
        };
        assert!(agree.parse().is_ok());

        let disagree = ArrangementForm {
            difficulty: Some("6".to_string()),
            rating_abrsm: Some("5".to_string()),
            ..with_title()
        };
        assert!(disagree.parse().is_err());

        // A non-integer ABRSM rounds to the nearest grade before comparison.
        let rounds = ArrangementForm {
            difficulty: Some("6".to_string()),
            rating_abrsm: Some("5.8".to_string()),
            ..with_title()
        };
        assert!(rounds.parse().is_ok());
    }

    #[test]
    fn other_scale_ratings_assemble_into_a_json_object() {
        let f = ArrangementForm {
            rating_henle: Some("5".to_string()),
            rating_rcm: Some("8".to_string()),
            ..with_title()
        };
        let parsed = f.parse().unwrap();
        let ratings = parsed.difficulty_ratings.expect("ratings present");
        assert_eq!(ratings.get("henle").and_then(|v| v.as_str()), Some("5"));
        assert_eq!(ratings.get("rcm").and_then(|v| v.as_str()), Some("8"));
        assert!(ratings.get("abrsm").is_none());
    }

    #[test]
    fn no_ratings_yield_none() {
        assert!(with_title().parse().unwrap().difficulty_ratings.is_none());
    }

    #[test]
    fn invalid_date_and_negative_numbers_are_rejected() {
        let bad_date = ArrangementForm {
            purchase_date: Some("nope".to_string()),
            ..with_title()
        };
        assert!(bad_date.parse().is_err());

        let negative = ArrangementForm {
            duration_seconds: Some("-5".to_string()),
            ..with_title()
        };
        assert!(negative.parse().is_err());
    }
}
