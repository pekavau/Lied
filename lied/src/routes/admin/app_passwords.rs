//! `/admin/app-passwords` — the caller's own WebDAV app passwords (issue #88).
//!
//! WebDAV is how musicians reach their parts, and an app password is how they
//! authenticate to it. Before this page the only way to mint one was
//! `POST /v1/app-passwords`, so a musician — who has no console workspace —
//! needed an API client before they could mount anything. This page needs no
//! org role: any logged-in user manages their own credentials here, through
//! the same `domain::app_password` functions and audit actions as `/v1`.
//!
//! Creating answers with the page itself rather than a redirect: the
//! plaintext exists only in this response (the server keeps a hash), so it
//! cannot survive a round trip.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::html;
use serde::Deserialize;
use tower_sessions::Session;
use uuid::Uuid;

use super::layout;
use crate::auth::csrf;
use crate::auth::extractors::AuthSession;
use crate::domain::app_password::{self, AppPasswordError};
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::user::User;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/app-passwords", get(list_page).post(create_submit))
        .route("/app-passwords/:id/revoke", post(revoke_submit))
}

/// What the page shows above the list after a create attempt.
enum Notice {
    None,
    /// A new password: its name and the plaintext, shown this once.
    Created {
        name: String,
        token: String,
    },
    Error(String),
}

async fn render(
    state: &AppState,
    session: &Session,
    user: &User,
    status: StatusCode,
    notice: Notice,
) -> Response {
    let token = csrf::ensure_token(session).await.unwrap_or_default();
    let passwords = match app_password::list_for_user(&state.db, user.id).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(%error, "failed to list app passwords");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<p class=\"error\">Could not load your app passwords.</p>"),
            )
                .into_response();
        }
    };
    let when = |at: chrono::DateTime<chrono::Utc>| at.format("%Y-%m-%d %H:%M UTC").to_string();

    let body = html! {
        h2 { "App passwords" }
        p {
            "An app password lets a WebDAV client — your tablet's reader, your "
            "computer's file manager, MuseScore — open your parts. Give each "
            "device its own, so losing one means revoking one. Sign in with "
            "your username " strong { (user.username) } " and the app password; "
            "your parts are under " code { "/orgs/<org>/" } " and your private "
            "library under " code { "/users/" (user.slug) "/library/" } "."
        }
        @match &notice {
            Notice::Created { name, token: plaintext } => {
                div class="notice" {
                    p {
                        "Created " strong { (name) } ". Copy it now — it is shown "
                        "only this once and cannot be retrieved later:"
                    }
                    p { code class="secret" { (plaintext) } }
                }
            }
            Notice::Error(message) => { p class="error" { (message) } }
            Notice::None => {}
        }
        form method="post" action="/admin/app-passwords" {
            (layout::csrf_field(&token))
            fieldset {
                legend { "New app password" }
                label { "Device name " input type="text" name="name" required placeholder="iPad in rehearsal room"; }
                button type="submit" { "Create" }
            }
        }
        @if passwords.is_empty() {
            p class="muted" { "You have no app passwords yet." }
        } @else {
            table {
                thead { tr { th { "Name" } th { "Starts with" } th { "Created" } th { "Last used" } th { "" } } }
                tbody {
                    @for p in &passwords {
                        tr {
                            td { (p.name) }
                            td { code { (p.prefix) "…" } }
                            td { (when(p.created_at)) }
                            td {
                                @match p.last_used_at {
                                    Some(at) => { (when(at)) }
                                    None => { span class="muted" { "never" } }
                                }
                            }
                            td {
                                @match p.revoked_at {
                                    Some(at) => { span class="muted" { "revoked " (when(at)) } }
                                    None => {
                                        form class="inline" method="post" action={ "/admin/app-passwords/" (p.id) "/revoke" } {
                                            (layout::csrf_field(&token))
                                            button type="submit" { "Revoke" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    (
        status,
        Html(layout::page("App passwords", &user.display_name, body).into_string()),
    )
        .into_response()
}

/// `GET /admin/app-passwords` — the caller's app passwords and a create form.
async fn list_page(
    State(state): State<AppState>,
    session: Session,
    auth: Option<AuthSession>,
) -> Response {
    let Some(AuthSession(user)) = auth else {
        return Redirect::to("/admin/login").into_response();
    };
    render(&state, &session, &user, StatusCode::OK, Notice::None).await
}

#[derive(Deserialize)]
struct CreateForm {
    name: String,
}

/// `POST /admin/app-passwords` — mint one and show its plaintext once.
async fn create_submit(
    State(state): State<AppState>,
    session: Session,
    auth: Option<AuthSession>,
    RequestId(request_id): RequestId,
    Form(form): Form<CreateForm>,
) -> Response {
    let Some(AuthSession(user)) = auth else {
        return Redirect::to("/admin/login").into_response();
    };
    let name = form.name.trim();
    if name.is_empty() {
        let notice = Notice::Error("Give the app password a name.".to_string());
        return render(&state, &session, &user, StatusCode::BAD_REQUEST, notice).await;
    }

    match app_password::issue(&state.db, user.id, name).await {
        Ok((summary, token)) => {
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(user.id),
                    org_id: None,
                    request_id: Some(request_id),
                },
                "auth.app_password_created",
                "app_password",
                Some(summary.id),
                serde_json::json!({ "name": summary.name, "prefix": summary.prefix }),
            )
            .await;
            let notice = Notice::Created {
                name: summary.name,
                token,
            };
            render(&state, &session, &user, StatusCode::CREATED, notice).await
        }
        Err(AppPasswordError::DuplicateName) => {
            let notice = Notice::Error(format!(
                "You already have an app password named \u{201c}{name}\u{201d} \
                 (revoked ones keep their name). Choose another."
            ));
            render(&state, &session, &user, StatusCode::CONFLICT, notice).await
        }
        Err(error) => {
            tracing::error!(%error, "failed to create app password");
            let notice = Notice::Error("Could not create the app password.".to_string());
            render(
                &state,
                &session,
                &user,
                StatusCode::INTERNAL_SERVER_ERROR,
                notice,
            )
            .await
        }
    }
}

/// `POST /admin/app-passwords/{id}/revoke` — revoke one of the caller's own.
/// Someone else's id, or one already revoked, is a 404: the query is scoped
/// to the caller, so another user's password is indistinguishable from none.
async fn revoke_submit(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
) -> Response {
    let Some(AuthSession(user)) = auth else {
        return Redirect::to("/admin/login").into_response();
    };
    match app_password::revoke(&state.db, id, user.id).await {
        Ok(true) => {
            audit(
                &state.db,
                &AuditContext {
                    actor_user_id: Some(user.id),
                    org_id: None,
                    request_id: Some(request_id),
                },
                "auth.app_password_revoked",
                "app_password",
                Some(id),
                serde_json::json!({}),
            )
            .await;
            Redirect::to("/admin/app-passwords").into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<p class=\"error\">App password not found.</p>"),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to revoke app password");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<p class=\"error\">Could not revoke the app password.</p>"),
            )
                .into_response()
        }
    }
}
