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

/// An authenticated user resolved from *either* the session cookie or an
/// `Authorization: Bearer <jwt>` header, for the `/v1` tree (CLAUDE.md:
/// "session cookie or bearer token"). Session is checked first since it's
/// cheaper (no JWT decode) and is the common case for the HTMX-adjacent
/// `/v1` calls the admin UI itself might make; bearer is checked second for
/// programmatic clients.
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
        // Try the session cookie first.
        if let Ok(session) = Session::from_request_parts(parts, state).await {
            if let Ok(Some(user_id)) = current_user_id(&session).await {
                if let Ok(Some(found)) = user::find_by_id(&state.db, user_id).await {
                    return Ok(BearerOrSession {
                        user: found,
                        org: None,
                    });
                }
            }
        }

        // Fall back to a bearer token.
        let auth_header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or(AppError::Unauthorized)?;

        let claims = state
            .jwt_keyring
            .verify(token)
            .map_err(|_| AppError::Unauthorized)?;

        let found = user::find_by_id(&state.db, claims.sub)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::Unauthorized)?;

        Ok(BearerOrSession {
            user: found,
            org: claims.org,
        })
    }
}
