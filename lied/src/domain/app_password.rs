//! AppPassword repository functions (CLAUDE.md Entities: AppPassword).
//!
//! See [`crate::auth::app_password`] for token generation/hashing; this
//! module owns the `app_password` table's CRUD plus the WebDAV
//! prefix-narrowed lookup.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of an `app_password` row. Never carries `hash` or
/// the plaintext token — only metadata, per CLAUDE.md's "shown ONCE on
/// creation" rule.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppPasswordSummary {
    pub id: Uuid,
    pub name: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A candidate row returned by the prefix-narrowed WebDAV lookup — carries
/// `hash` for the caller to argon2-verify against. Never serialized.
#[derive(Debug, Clone)]
pub struct AppPasswordCandidate {
    pub id: Uuid,
    pub user_id: Uuid,
    pub hash: String,
}

#[derive(thiserror::Error, Debug)]
pub enum AppPasswordError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("an app password with this name already exists")]
    DuplicateName,
}

fn map_insert_error(err: sqlx::Error) -> AppPasswordError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("app_password_user_id_name_key") {
            return AppPasswordError::DuplicateName;
        }
    }
    AppPasswordError::Database(err)
}

/// Insert a new app password row. Returns the summary (no hash/plaintext —
/// the caller already holds the plaintext from generation and is
/// responsible for returning it to the user exactly once).
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    name: &str,
    hash: &str,
    prefix: &str,
) -> Result<AppPasswordSummary, AppPasswordError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO app_password (id, user_id, name, hash, prefix)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING
            id, name, prefix,
            created_at as "created_at: DateTime<Utc>",
            last_used_at as "last_used_at: DateTime<Utc>",
            revoked_at as "revoked_at: DateTime<Utc>"
        "#,
        id,
        user_id,
        name,
        hash,
        prefix,
    )
    .fetch_one(pool)
    .await
    .map_err(map_insert_error)?;

    Ok(AppPasswordSummary {
        id: row.id,
        name: row.name,
        prefix: row.prefix,
        created_at: row.created_at,
        last_used_at: row.last_used_at,
        revoked_at: row.revoked_at,
    })
}

/// List all app passwords (active and revoked) for a user, newest first.
pub async fn list_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Vec<AppPasswordSummary>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            id, name, prefix,
            created_at as "created_at: DateTime<Utc>",
            last_used_at as "last_used_at: DateTime<Utc>",
            revoked_at as "revoked_at: DateTime<Utc>"
        FROM app_password
        WHERE user_id = $1
        ORDER BY created_at DESC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| AppPasswordSummary {
            id: row.id,
            name: row.name,
            prefix: row.prefix,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
            revoked_at: row.revoked_at,
        })
        .collect())
}

/// Revoke an app password (sets `revoked_at`). Scoped to `user_id` so a
/// user can only revoke their own. Returns `true` if a row was updated.
pub async fn revoke(pool: &PgPool, id: Uuid, user_id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        UPDATE app_password
        SET revoked_at = now()
        WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
        "#,
        id,
        user_id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Candidate rows for WebDAV Basic-auth resolution: active app passwords
/// for `user_id` whose `prefix` matches. Typically returns exactly one row;
/// the caller argon2-verifies the plaintext against each candidate's hash.
pub async fn find_candidates_by_prefix(
    pool: &PgPool,
    user_id: Uuid,
    prefix: &str,
) -> Result<Vec<AppPasswordCandidate>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT id, user_id, hash
        FROM app_password
        WHERE user_id = $1 AND prefix = $2 AND revoked_at IS NULL
        "#,
        user_id,
        prefix,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| AppPasswordCandidate {
            id: row.id,
            user_id: row.user_id,
            hash: row.hash,
        })
        .collect())
}

/// Stamp `last_used_at = now()` after a successful WebDAV authentication.
pub async fn touch_last_used(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"UPDATE app_password SET last_used_at = now() WHERE id = $1"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}
