//! WebDAV `Authorization: Basic` resolution via app passwords (CLAUDE.md
//! AppPassword entity: "WebDAV `Authorization: Basic
//! base64(username:token)` resolves by...").
//!
//! This module implements the resolution algorithm only (decode → look up
//! user → prefix-narrow candidates → argon2 verify → touch `last_used_at`);
//! [`crate::routes::webdav`] wires it into actual axum middleware in front
//! of the `/orgs` and `/users/.../library` route trees.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::app_password::{prefix_of, verify_token};
use crate::domain::app_password as app_password_repo;
use crate::domain::audit_log::{audit, AuditContext};

#[derive(Debug, Clone)]
pub struct AuthenticatedWebDavUser {
    pub user_id: Uuid,
    pub username: String,
}

#[derive(thiserror::Error, Debug)]
pub enum WebDavAuthError {
    #[error("missing or malformed Authorization header")]
    MissingOrMalformed,
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

/// Decode an `Authorization: Basic <base64>` header value into
/// `(username, token)`.
fn decode_basic_auth(header_value: &str) -> Option<(String, String)> {
    let encoded = header_value.strip_prefix("Basic ")?;
    let decoded = STANDARD.decode(encoded).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (username, token) = text.split_once(':')?;
    Some((username.to_string(), token.to_string()))
}

/// Resolve an `Authorization` header value into an authenticated WebDAV
/// user via app-password Basic auth. On success, stamps `last_used_at` on
/// the matched app password and audits `auth.app_password_used`. On
/// failure, audits nothing by itself — the caller (see
/// [`crate::routes::webdav`]) decides whether a failed WebDAV auth attempt
/// is worth its own row to satisfy "every auth event" from CLAUDE.md.
pub async fn authenticate(
    pool: &PgPool,
    authorization_header: Option<&str>,
    request_id: Option<Uuid>,
) -> Result<AuthenticatedWebDavUser, WebDavAuthError> {
    let header_value = authorization_header.ok_or(WebDavAuthError::MissingOrMalformed)?;
    let (username, token) =
        decode_basic_auth(header_value).ok_or(WebDavAuthError::MissingOrMalformed)?;

    let candidate_user = sqlx::query!(
        r#"SELECT id, username::text as "username!" FROM "user" WHERE username = $1"#,
        username,
    )
    .fetch_optional(pool)
    .await?;

    let Some(candidate_user) = candidate_user else {
        return Err(WebDavAuthError::InvalidCredentials);
    };

    let prefix = prefix_of(&token);
    let candidates =
        app_password_repo::find_candidates_by_prefix(pool, candidate_user.id, &prefix).await?;

    for candidate in candidates {
        if verify_token(&token, &candidate.hash).unwrap_or(false) {
            app_password_repo::touch_last_used(pool, candidate.id).await?;

            audit(
                pool,
                &AuditContext {
                    actor_user_id: Some(candidate_user.id),
                    org_id: None,
                    request_id,
                },
                "auth.app_password_used",
                "app_password",
                Some(candidate.id),
                serde_json::json!({ "username": candidate_user.username }),
            )
            .await;

            return Ok(AuthenticatedWebDavUser {
                user_id: candidate_user.id,
                username: candidate_user.username,
            });
        }
    }

    Err(WebDavAuthError::InvalidCredentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_basic_auth_parses_username_and_token() {
        let encoded = STANDARD.encode("alice:lied_sometoken");
        let header = format!("Basic {encoded}");
        let (username, token) = decode_basic_auth(&header).unwrap();
        assert_eq!(username, "alice");
        assert_eq!(token, "lied_sometoken");
    }

    #[test]
    fn decode_basic_auth_rejects_non_basic_scheme() {
        assert!(decode_basic_auth("Bearer sometoken").is_none());
    }

    #[test]
    fn decode_basic_auth_rejects_malformed_base64() {
        assert!(decode_basic_auth("Basic not-valid-base64!!!").is_none());
    }

    #[test]
    fn decode_basic_auth_rejects_missing_colon() {
        let encoded = STANDARD.encode("no-colon-here");
        let header = format!("Basic {encoded}");
        assert!(decode_basic_auth(&header).is_none());
    }
}
