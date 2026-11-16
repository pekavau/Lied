//! `/admin/orgs/{org}/settings` — the owner's org settings (#119).
//!
//! One setting so far: whether musicians read the whole archive (every
//! arrangement, voice and full score, read-only) or only their assigned
//! parts. Whether that is wanted depends on the ensemble, so it is the
//! owner's decision ("Org settings" in the permission matrix; a system admin
//! is owner-equivalent). The same change is `PATCH /v1/orgs/{orgId}` with
//! `musiciansReadArchive`, audited as the same event.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Router};
use maud::html;
use serde::Deserialize;
use tower_sessions::Session;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::organization;
use crate::listing::{self, check_if_match};
use crate::routes::admin::arrangements::{audit_ctx, csrf_token};
use crate::routes::admin::console::{self, Section};
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/orgs/:org_id/settings",
        get(settings_page).post(update_settings),
    )
}

async fn settings_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    session: Session,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_change_settings() {
        return console::section_forbidden(&ctx);
    }
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };
    let org = ctx.org();
    let body = html! {
        form method="post" action=(format!("/admin/orgs/{org_id}/settings")) {
            (layout::csrf_field(&token))
            input type="hidden" name="expected_version" value=(org.updated_at.timestamp_millis());
            fieldset {
                legend { "Archive access for musicians" }
                label {
                    input type="checkbox" name="musicians_read_archive" value="on"
                        checked[org.musicians_read_archive];
                    " Musicians may read the whole archive"
                }
                p class="muted" {
                    "Off: a musician sees only the parts assigned to them. On: every "
                    "musician may read every arrangement, voice and full score, over "
                    "WebDAV and the app, without changing anything. Guests always see "
                    "only their assigned parts."
                }
            }
            button type="submit" { "Save" }
        }
    };
    Html(console::console_page(&ctx, Section::Settings, body).into_string()).into_response()
}

#[derive(Deserialize)]
struct SettingsForm {
    expected_version: Option<String>,
    /// Present (`on`) when the box is ticked; browsers omit an unticked one.
    musicians_read_archive: Option<String>,
}

async fn update_settings(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    RequestId(request_id): RequestId,
    Form(form): Form<SettingsForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.can_change_settings() {
        return console::section_forbidden(&ctx);
    }
    let back = format!("/admin/orgs/{org_id}/settings");
    let current = ctx.org().clone();
    if check_if_match(form.expected_version.as_deref(), current.updated_at).is_err() {
        return console::precondition_page(&ctx, Section::Settings, "setting", &back);
    }
    let value = form.musicians_read_archive.is_some();
    if value == current.musicians_read_archive {
        return Redirect::to(&back).into_response();
    }
    match organization::set_musicians_read_archive(
        &state.db,
        org_id,
        Some(listing::version_of(current.updated_at)),
        value,
    )
    .await
    {
        Ok(Some(updated)) => {
            crate::routes::orgs::audit_settings_change(
                &state,
                &audit_ctx(&ctx, request_id),
                &current,
                &updated,
            )
            .await;
            Redirect::to(&back).into_response()
        }
        // The org was read a moment ago and orgs are only hard-deleted, so a
        // miss means another edit got in first.
        Ok(None) => console::precondition_page(&ctx, Section::Settings, "setting", &back),
        Err(error) => {
            tracing::error!(%error, "failed to update org settings");
            console::error_page(
                &ctx,
                Section::Settings,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the settings.",
            )
        }
    }
}
