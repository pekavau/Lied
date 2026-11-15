//! Axum extractors that resolve an authenticated [`crate::domain::user::User`]
//! from either a session cookie or a bearer token (CLAUDE.md: "REST API
//! uses session cookies (web client) or bearer tokens (programmatic)").
//!
//! Both extractors are `async` `FromRequestParts` impls so handlers simply
//! take them as parameters; missing/invalid credentials short-circuit to
//! [`AppError::Unauthorized`] before the handler body ever runs.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use tower_sessions::Session;
use uuid::Uuid;

use crate::auth::csrf;
use crate::auth::session::current_user_id;
use crate::domain::user::{self, User};
use crate::error::AppError;
use crate::state::AppState;

/// An authenticated user resolved from the `/admin` tree's session cookie
/// only. Used by `/admin` handlers that require a logged-in user (every
/// admin route except the login page/form itself).
pub struct AuthSession(pub User);

#[async_trait::async_trait]
impl FromRequestParts<AppState> for AuthSession {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|_| AppError::Unauthorized)?;

        let user_id = current_user_id(&session)
            .await
            .map_err(|_| AppError::Unauthorized)?
            .ok_or(AppError::Unauthorized)?;

        let found = user::find_by_id(&state.db, user_id)
            .await
            .map_err(AppError::from)?;

        found.map(AuthSession).ok_or(AppError::Unauthorized)
    }
}

/// An authenticated user resolved from *either* an `Authorization: Bearer
/// <jwt>` header or the session cookie, for the `/v1` tree (CLAUDE.md:
/// "session cookie or bearer token").
///
/// **The `Authorization` header wins.** If the request carries one, it alone
/// decides: a `Bearer` credential (scheme matched case-insensitively, per
/// RFC 7235) authenticates the request, and anything else — an invalid
/// token, another scheme such as `Basic`, a malformed header — is a 401,
/// never a fall-through to the cookie. A bearer request is exempt from CSRF:
/// a cross-site page cannot attach an `Authorization` header (that needs a
/// CORS preflight `/v1` never grants), so it is not forgeable the way a
/// cookie request is.
///
/// **Cookie writes need the CSRF token** (issue #72). Without an
/// `Authorization` header the session cookie is tried, and a state-changing
/// method (anything but `GET`/`HEAD`/`OPTIONS`/`TRACE`) must then carry the
/// session's token in `HX-CSRF`, or the request is a 403 Problem Details.
/// The check lives here, rather than in a `/v1` middleware, because this is
/// where the request is known to have authenticated by cookie: a middleware
/// would have to re-derive which path the extractor takes, and a request
/// carrying both a cookie and a bearer would be judged by a different rule
/// than the one that authenticates it. Every authenticated `/v1` handler
/// takes this extractor, and a body extractor always runs after the parts
/// extractors, so no cookie-authenticated write reaches a handler (or has
/// its body parsed) without the check. `POST /v1/tokens`, the one handler
/// that reads the session directly, takes [`SessionCsrf`] instead.
pub struct BearerOrSession {
    pub user: User,
    pub org: Option<Uuid>,
}

/// The credential of the request's `Authorization` header: `Ok(None)` when
/// there is none, the token of a `Bearer` credential, or a 401 for any other
/// scheme or a malformed header (see [`BearerOrSession`]).
fn bearer_credential(parts: &Parts) -> Result<Option<&str>, AppError> {
    let Some(value) = parts.headers.get(axum::http::header::AUTHORIZATION) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| AppError::Unauthorized)?;
    let (scheme, token) = value
        .trim()
        .split_once(|c: char| c.is_ascii_whitespace())
        .ok_or(AppError::Unauthorized)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AppError::Unauthorized);
    }
    Ok(Some(token.trim_start()))
}

/// The request's session. Failing to extract it means the session layer is
/// not mounted — a wiring bug, so a 500, not a 401.
async fn request_session(parts: &mut Parts, state: &AppState) -> Result<Session, AppError> {
    Session::from_request_parts(parts, state)
        .await
        .map_err(|(_, message)| AppError::Internal(anyhow::anyhow!("no session layer: {message}")))
}

/// The logged-in user id of `session`. A session-store failure is a 500
/// (the store is down), not a 401 (the caller is not logged in).
async fn session_user_id(session: &Session) -> Result<Option<Uuid>, AppError> {
    current_user_id(session)
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!("reading the session: {error}")))
}

#[async_trait::async_trait]
impl FromRequestParts<AppState> for BearerOrSession {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(token) = bearer_credential(parts)? {
            let claims = state
                .jwt_keyring
                .verify(token)
                .map_err(|_| AppError::Unauthorized)?;

            let found = user::find_by_id(&state.db, claims.sub)
                .await
                .map_err(AppError::from)?
                .ok_or(AppError::Unauthorized)?;

            return Ok(BearerOrSession {
                user: found,
                org: claims.org,
            });
        }

        let session = request_session(parts, state).await?;
        let user_id = session_user_id(&session)
            .await?
            .ok_or(AppError::Unauthorized)?;

        // Authenticated by cookie: a write must prove it came from a page
        // that could read the session's CSRF token. Checked before anything
        // else is done on the request's behalf.
        if csrf::is_state_changing(&parts.method) {
            csrf::verify_header(&session, &parts.headers).await?;
        }

        let found = user::find_by_id(&state.db, user_id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::Unauthorized)?;

        Ok(BearerOrSession {
            user: found,
            org: None,
        })
    }
}

/// A CSRF guard for a handler that reads the session itself rather than
/// through [`BearerOrSession`] — `POST /v1/tokens`, which mints from either
/// the session or a username/password body. If the request is a
/// state-changing one riding a logged-in session, the `HX-CSRF` header must
/// carry the session's token, or the request is a 403. Without a logged-in
/// session it passes: there is nothing for a forged request to ride.
///
/// Being a parts extractor, it runs before the handler's body extractor, so
/// a forged form POST is refused as a CSRF failure (403) rather than as an
/// unsupported body (415).
pub struct SessionCsrf;

#[async_trait::async_trait]
impl FromRequestParts<AppState> for SessionCsrf {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if !csrf::is_state_changing(&parts.method) {
            return Ok(SessionCsrf);
        }
        let session = request_session(parts, state).await?;
        if session_user_id(&session).await?.is_some() {
            csrf::verify_header(&session, &parts.headers).await?;
        }
        Ok(SessionCsrf)
    }
}
