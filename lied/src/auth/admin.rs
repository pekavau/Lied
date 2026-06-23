//! `create_admin`: bootstrap the first `is_system_admin` user (CLAUDE.md
//! Auth & access; backs the `lied-server create-admin` CLI subcommand).
//!
//! Lives in the library (not the binary) per CLAUDE.md's "thin binary"
//! convention, and so it's directly testable without spawning a process.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::password::{hash_password, PasswordError};
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::user::{self, UserError};

#[derive(thiserror::Error, Debug)]
pub enum CreateAdminError {
    #[error("failed to hash password")]
    Password(#[from] PasswordError),
    #[error(transparent)]
    User(#[from] UserError),
}

pub struct CreateAdminParams<'a> {
    pub username: &'a str,
    pub email: Option<&'a str>,
    pub display_name: &'a str,
    pub password: &'a str,
}

/// Create a new user with `is_system_admin = true`. Slug is derived from
/// the username via [`user::slugify`]. Audits `auth.create_admin` on
/// success (the password is hashed before this function is even called by
/// the audit site below — the plaintext never reaches the payload).
pub async fn create_admin(
    pool: &PgPool,
    params: CreateAdminParams<'_>,
) -> Result<user::User, CreateAdminError> {
    let password_hash = hash_password(params.password)?;
    let slug = user::slugify(params.username);
    let id = Uuid::now_v7();

    let created = user::create(
        pool,
        id,
        &slug,
        params.username,
        params.email,
        Some(&password_hash),
        params.display_name,
        true,
        None,
    )
    .await?;

    audit(
        pool,
        &AuditContext {
            actor_user_id: Some(created.id),
            org_id: None,
            request_id: None,
        },
        "auth.create_admin",
        "user",
        Some(created.id),
        json!({ "username": created.username, "slug": created.slug }),
    )
    .await;

    Ok(created)
}
