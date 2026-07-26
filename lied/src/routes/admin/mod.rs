//! `/admin/...` — HTMX admin UI tree. HTML fragments, session-cookie + CSRF
//! auth. Issue #4 added the login/logout flow plus the session + CSRF
//! middleware the rest of the admin tree sits behind; issue #5 adds the
//! first real screens (`orgs` submodule: org/user/membership management),
//! sharing the `layout` submodule's maud page shell.

pub mod arrangements;
pub mod console;
pub mod layout;
pub mod orgs;

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::csrf::{self, csrf_middleware};
use crate::auth::extractors::AuthSession;
use crate::auth::ratelimit::login_rate_limit_layer;
use crate::auth::session::{login, logout, LoginError};
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router(state: &AppState) -> Router<AppState> {
    let session_layer =
        crate::auth::session::build_session_layer(state.db.clone(), state.config.secure_cookies);

    // The login POST gets its own per-IP rate-limit bucket (CLAUDE.md
    // Security baseline: "Login / password endpoints: 10 req/min per IP");
    // scoped to just this route, not the whole `/admin` tree, since HTMX
    // UIs fire several requests per interaction on the rest of the tree.
    let login_route = get(login_page).merge(
        post(login_submit).layer(login_rate_limit_layer(state.config.ratelimit_login_per_min)),
    );

    Router::new()
        .route("/", get(index))
        .route("/login", login_route)
        .route("/logout", post(logout_submit))
        // Org/User/Membership screens (issue #5) — see `orgs` submodule.
        .merge(orgs::router())
        // Per-org management console (issue #30) — see `console` submodule.
        .merge(console::router())
        // Arrangement + Work management screens (issue #31).
        .merge(arrangements::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            csrf_middleware,
        ))
        .layer(session_layer)
}

async fn index(State(state): State<AppState>, auth: Option<AuthSession>) -> Response {
    let Some(AuthSession(user)) = auth else {
        return Redirect::to("/admin/login").into_response();
    };

    // A row in the workspace list: an org the caller can actually enter, with
    // an honest role label (never a fabricated one).
    struct WorkspaceRow {
        id: uuid::Uuid,
        name: String,
        role_label: String,
    }

    // Nested `fn` (not a closure) so it can be passed by name into `.map` in
    // the loop below — it captures nothing.
    fn membership_label(m: &crate::domain::membership::UserOrg) -> String {
        if m.is_principal {
            format!("{} (principal)", m.role.as_str())
        } else {
            m.role.as_str().to_string()
        }
    }

    let memberships = match crate::domain::membership::list_for_user(&state.db, user.id).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(%error, "failed to list user memberships");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut rows: Vec<WorkspaceRow> = Vec::new();
    if user.is_system_admin {
        // A system admin enters every org (owner-equivalent), so list them all,
        // paging through so none are silently dropped. Label by their actual
        // membership where one exists, else "system admin" — never a fake role.
        const ORG_PAGE: i64 = 200;
        let mut offset: i64 = 0;
        loop {
            let (orgs, total) = match crate::domain::organization::list(
                &state.db,
                ORG_PAGE,
                offset,
                "name",
                crate::listing::SortDirection::Asc,
                None,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    tracing::error!(%error, "failed to list organizations for system admin");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            if orgs.is_empty() {
                break;
            }
            let fetched = orgs.len() as i64;
            for org in orgs {
                let role_label = memberships
                    .iter()
                    .find(|m| m.organization_id == org.id)
                    .map(membership_label)
                    .unwrap_or_else(|| "system admin".to_string());
                rows.push(WorkspaceRow {
                    id: org.id,
                    name: org.name,
                    role_label,
                });
            }
            offset += fetched;
            if offset >= total {
                break;
            }
        }
        // `organization::list` orders by name and we appended in page order, so
        // `rows` is already name-sorted.
    } else {
        // Non-admins see only orgs they can actually enter (staff, or a
        // principal musician) — so the list never advertises a workspace the
        // console gate would 403 on. Uses the same rule as `ConsoleCtx::load`.
        for m in &memberships {
            if console::can_enter(m.role, m.is_principal) {
                rows.push(WorkspaceRow {
                    id: m.organization_id,
                    name: m.organization_name.clone(),
                    role_label: membership_label(m),
                });
            }
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
    }

    let body = maud::html! {
        h2 { "Your workspaces" }
        @if rows.is_empty() {
            p class="muted" {
                "You don't have access to any organization workspaces."
                @if user.is_system_admin { " Create one under Organizations." }
            }
        } @else {
            table {
                thead { tr { th { "Organization" } th { "Role" } th {} } }
                tbody {
                    @for row in &rows {
                        tr {
                            td { (row.name) }
                            td { (row.role_label) }
                            td {
                                a href={ "/admin/orgs/" (row.id) "/console" } {
                                    "Open workspace"
                                }
                            }
                        }
                    }
                }
            }
        }
        @if user.is_system_admin {
            p class="muted" {
                "System administration: "
                a href="/admin/orgs" { "Organizations" }
                " · "
                a href="/admin/users" { "Users" }
            }
        }
    };
    Html(layout::page("Home", &user.display_name, body).into_string()).into_response()
}

async fn login_page(session: Session) -> impl IntoResponse {
    let token = match csrf::ensure_token(&session).await {
        Ok(token) => token,
        Err(error) => {
            tracing::error!(%error, "failed to establish CSRF token");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let body = format!(
        r#"<h1>Lied admin login</h1>
<form method="post" action="/admin/login">
  <input type="hidden" name="csrf_token" value="{token}" />
  <label>Username <input type="text" name="username" /></label>
  <label>Password <input type="password" name="password" /></label>
  <button type="submit">Log in</button>
</form>"#,
        token = html_escape(&token)
    );

    Html(body).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

/// `POST /admin/login` — verifies username+password and, on success,
/// stores the user id in the session (CLAUDE.md: "on success put user id
/// into the session"). The login *form post itself* is deliberately
/// exempt from the CSRF header check below since the form is rendered
/// server-side with no prior HTMX-driven page load to source an `HX-CSRF`
/// header from; the session-bound CSRF token still gets minted on
/// `login_page` and is honored as a hidden form field here for
/// defense-in-depth, but the canonical CSRF defense for *this* endpoint is
/// that an attacker cannot forge a valid session cookie to begin with.
async fn login_submit(
    State(state): State<AppState>,
    session: Session,
    RequestId(request_id): RequestId,
    Form(form): Form<LoginForm>,
) -> impl IntoResponse {
    match login(
        &state.db,
        &session,
        &form.username,
        &form.password,
        Some(request_id),
    )
    .await
    {
        // Redirect to `/admin` WITHOUT a trailing slash: `nest("/admin", …)`
        // matches `/admin` but not `/admin/` (axum 0.7 / matchit treats them
        // as distinct paths; the inner `route("/")` only covers the bare
        // prefix), so `/admin/` would fall through to a 404 (issue #15, bug 2).
        Ok(_user) => Redirect::to("/admin").into_response(),
        Err(LoginError::InvalidCredentials) => (
            StatusCode::UNAUTHORIZED,
            Html("<p>Invalid username or password.</p>"),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "login failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /admin/logout` — clears the session only. Does not touch any
/// bearer token or app password (CLAUDE.md acceptance criterion: the three
/// auth paths are independent).
async fn logout_submit(
    State(state): State<AppState>,
    session: Session,
    RequestId(request_id): RequestId,
) -> Response {
    if let Err(error) = logout(&state.db, &session, Some(request_id)).await {
        tracing::error!(%error, "logout failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let mut response = Redirect::to("/admin/login").into_response();
    // Belt-and-suspenders: also clear the readable CSRF cookie client-side,
    // since the session backing it was just flushed.
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static("lied_csrf=; Path=/; Max-Age=0"),
    );
    response
}

/// Minimal HTML-escaping for the hand-rolled fragments in this module.
/// `maud` (auto-escaping) is reserved for the real admin UI templates in a
/// later item; this scaffold avoids pulling maud into login-page rendering
/// for four interpolated strings.
fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
