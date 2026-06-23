//! Postgres-backed web sessions (CLAUDE.md "Auth & access" /
//! `tower-sessions`).
//!
//! Wires `tower_sessions_sqlx_store::PostgresStore` (over the
//! `tower_sessions.session` table created by migration `0002`) into a
//! `tower_sessions::SessionManagerLayer`, and provides the login/logout
//! handlers for the `/admin` tree.
//!
//! Per CLAUDE.md's infrastructure-pluggability rule, nothing in this module
//! (or anywhere else in the app) JOINs the `tower_sessions.session` table
//! with domain tables — all access goes through the `SessionStore` trait
//! that `PostgresStore` implements. The only thing we read out of a
//! [`tower_sessions::Session`] is the `user_id` we ourselves put there.

use serde_json::json;
use sqlx::PgPool;
use tower_sessions::cookie::SameSite;
use tower_sessions::{Expiry, Session, SessionManagerLayer};
use tower_sessions_sqlx_store::PostgresStore;
use uuid::Uuid;

use crate::auth::password::{verify_dummy, verify_password};
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::user;

/// Session key under which the authenticated user's id is stored.
pub const SESSION_USER_ID_KEY: &str = "user_id";
/// Cookie name for the session cookie. Distinct from the default
/// `tower-sessions`'s `id` name so it's unambiguous in browser devtools.
pub const SESSION_COOKIE_NAME: &str = "lied_session";

/// Build the session store + manager layer. `secure` should come from
/// `config.secure_cookies` (see CLAUDE.md note in `config.rs`).
///
/// Does not call `PostgresStore::migrate()` — the session table is created
/// by our own numbered migration (`0002_tower_sessions.sql`), not by the
/// crate at runtime, per the infra-pluggability rule's "every schema object
/// lives in our own numbered /migrations set" decision.
pub fn build_session_layer(pool: PgPool, secure: bool) -> SessionManagerLayer<PostgresStore> {
    let store = PostgresStore::new(pool);

    SessionManagerLayer::new(store)
        .with_name(SESSION_COOKIE_NAME)
        .with_http_only(true)
        .with_same_site(SameSite::Lax)
        .with_secure(secure)
        .with_expiry(Expiry::OnInactivity(
            tower_sessions::cookie::time::Duration::days(7),
        ))
}

#[derive(thiserror::Error, Debug)]
pub enum LoginError {
    #[error("invalid username or password")]
    InvalidCredentials,
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("session error")]
    Session(#[from] tower_sessions::session::Error),
}

/// Verify username/password and, on success, store the user id in the
/// session. Always audits the attempt (`auth.login_succeeded` /
/// `auth.login_failed`) regardless of outcome.
///
/// `request_id` is threaded through for audit correlation; `None` is fine
/// (the column is nullable) when called from a context without one (e.g.
/// the `/v1/tokens` username+password path).
pub async fn login(
    pool: &PgPool,
    session: &Session,
    username: &str,
    password: &str,
    request_id: Option<Uuid>,
) -> Result<user::User, LoginError> {
    let found = user::find_by_username(pool, username).await?;

    let outcome = match found.as_ref().and_then(|c| c.password_hash.as_deref()) {
        Some(hash) => verify_password(password, hash).unwrap_or(false),
        // No such user, or an account with no local password (OIDC-only):
        // burn an equivalent argon2 cost so response latency can't reveal
        // which case it was (username enumeration). See `verify_dummy`.
        None => {
            verify_dummy();
            false
        }
    };

    if !outcome {
        audit(
            pool,
            &AuditContext {
                actor_user_id: found.as_ref().map(|u| u.id),
                org_id: None,
                request_id,
            },
            "auth.login_failed",
            "user",
            found.as_ref().map(|u| u.id),
            json!({ "username": username }),
        )
        .await;
        return Err(LoginError::InvalidCredentials);
    }

    let candidate = found.expect("outcome is true only when found.is_some()");

    // Session fixation defense: rotate the session id on the anonymous →
    // authenticated transition, so a session id an attacker planted in the
    // victim's browser before login can never be reused to ride the
    // authenticated session. Must happen before we write the user id.
    session.cycle_id().await?;
    session.insert(SESSION_USER_ID_KEY, candidate.id).await?;

    audit(
        pool,
        &AuditContext {
            actor_user_id: Some(candidate.id),
            org_id: None,
            request_id,
        },
        "auth.login_succeeded",
        "user",
        Some(candidate.id),
        json!({ "username": candidate.username }),
    )
    .await;

    Ok(user::User {
        id: candidate.id,
        slug: candidate.slug,
        username: candidate.username,
        email: candidate.email,
        display_name: candidate.display_name,
        is_system_admin: candidate.is_system_admin,
        created_at: candidate.created_at,
        updated_at: candidate.updated_at,
    })
}

/// Clear the session (logout). Audits `auth.logout`. Does not touch any
/// app password or bearer token — the three auth paths are independent
/// (CLAUDE.md acceptance criterion).
pub async fn logout(
    pool: &PgPool,
    session: &Session,
    request_id: Option<Uuid>,
) -> Result<(), tower_sessions::session::Error> {
    let user_id: Option<Uuid> = session.get(SESSION_USER_ID_KEY).await?;

    session.flush().await?;

    audit(
        pool,
        &AuditContext {
            actor_user_id: user_id,
            org_id: None,
            request_id,
        },
        "auth.logout",
        "user",
        user_id,
        json!({}),
    )
    .await;

    Ok(())
}

/// Resolve the currently authenticated user (if any) from the session.
pub async fn current_user_id(
    session: &Session,
) -> Result<Option<Uuid>, tower_sessions::session::Error> {
    session.get(SESSION_USER_ID_KEY).await
}
