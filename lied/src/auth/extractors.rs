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
/// **Bearer wins.** A request presenting a bearer token is authenticated by
/// that token alone — an invalid one is a 401, never a fall-through to the
/// cookie — and is exempt from CSRF: a cross-site page cannot attach an
/// `Authorization` header (that needs a CORS preflight `/v1` never grants),
/// so a bearer request is not forgeable the way a cookie request is.
///
/// **Cookie writes need the CSRF token** (issue #72). Without a bearer
/// header the session cookie is tried, and a state-changing method (anything
/// but `GET`/`HEAD`/`OPTIONS`) must then carry the session's token in
/// `HX-CSRF`, or the request is a 403 Problem Details. The check lives here,
/// rather than in a `/v1` middleware, because this is where the request is
/// known to have authenticated by cookie: a middleware would have to
/// re-derive which path the extractor takes, and a request carrying both a
/// cookie and a bearer would be judged by a different rule than the one that
/// authenticates it. Every authenticated `/v1` handler takes this extractor,
/// so no cookie-authenticated write can reach a handler without the check;
/// `POST /v1/tokens`, the one handler that reads the session directly,
/// applies the same [`csrf::verify_header`] itself.
pub struct BearerOrSession {
    pub user: User,
    pub org: Option<Uuid>,
}

#[async_trait::async_trait]
impl FromRequestParts<AppState> for BearerOrSession {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        if let Some(token) = bearer {
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

        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|_| AppError::Unauthorized)?;
        let user_id = current_user_id(&session)
            .await
            .map_err(|_| AppError::Unauthorized)?
            .ok_or(AppError::Unauthorized)?;
        let found = user::find_by_id(&state.db, user_id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::Unauthorized)?;

        // Authenticated by cookie: a write must prove it came from a page
        // that could read the session's CSRF token.
        if csrf::is_state_changing(&parts.method) {
            csrf::verify_header(&session, &parts.headers).await?;
        }

        Ok(BearerOrSession {
            user: found,
            org: None,
        })
    }
}
