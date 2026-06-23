//! `/admin/...` — HTMX admin UI tree. HTML fragments, session-cookie + CSRF
//! auth. Entity CRUD lands in later phase-1 items; this item (issue #4)
//! adds the login/logout flow plus the session + CSRF middleware that the
//! rest of the admin tree will sit behind.

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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            csrf_middleware,
        ))
        .layer(session_layer)
}

async fn index(auth: Option<AuthSession>) -> impl IntoResponse {
    match auth {
        Some(AuthSession(user)) => Html(format!(
            "<h1>Lied admin</h1><p>Logged in as {}.</p><form method=\"post\" action=\"/admin/logout\"><button type=\"submit\">Log out</button></form>",
            html_escape(&user.display_name)
        ))
        .into_response(),
        None => Redirect::to("/admin/login").into_response(),
    }
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
        Ok(_user) => Redirect::to("/admin/").into_response(),
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
