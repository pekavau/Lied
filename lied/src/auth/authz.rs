//! Authorization helpers enforcing the CLAUDE.md Permission matrix.
//!
//! Issue #5 (Org/User/Membership management) is the first item to need
//! authorization beyond "is there a logged-in user" — every later phase-1
//! item that gates a route by org role reuses these helpers, so the shape
//! is deliberately generic rather than copy-pasted per handler.
//!
//! Two checks, matching the two halves of CLAUDE.md's authorization model:
//! - **Instance-level**: [`require_system_admin`] — `User.is_system_admin`
//!   short-circuits instance ops (org/user provisioning). There is no
//!   "system-wide role" beyond this single boolean.
//! - **Org-scoped**: [`require_org_role`] — looks up the caller's
//!   `Membership` in the target org and checks its `role` against a
//!   minimum-seniority threshold using the Permission matrix's column
//!   ordering (`owner` > `archivist`/`conductor` > `musician`, *not* a
//!   single total order — see [`Role::at_least`] for why a closed
//!   `matches!` table is used instead of a numeric rank).
//!
//! Both are plain `async fn`s taking the already-resolved [`BearerOrSession`]
//! (or [`AuthSession`]) plus an org id, rather than a custom extractor, so a
//! handler can resolve the *target* org id from a path/body param first
//! (often only known after the request is partially parsed) and call the
//! check inline — an extractor would have to guess at a fixed param name.

use uuid::Uuid;

use crate::auth::extractors::{AuthSession, BearerOrSession};
use crate::domain::membership::{self, Role};
use crate::domain::user::User;
use crate::error::AppError;
use crate::state::AppState;

impl Role {
    /// Whether this role meets or exceeds `minimum` for the *single*
    /// "manage members & roles" capability, which per the Permission matrix
    /// is `owner`-only. Phase 1 has exactly one ordered capability tier
    /// (owner > everyone else) plus several capabilities shared by
    /// {owner, archivist} or {owner, archivist, conductor} that are NOT a
    /// strict chain (e.g. archivist can manage tags but conductor cannot,
    /// while conductor can do global annotations but archivist cannot) — so
    /// a single numeric rank would misrepresent the matrix. This helper
    /// covers the owner-only chain; other capability checks should match
    /// CLAUDE.md's Permission matrix table directly with a `matches!` rather
    /// than extending this into a general lattice.
    pub fn at_least(&self, minimum: Role) -> bool {
        match minimum {
            Role::Owner => *self == Role::Owner,
            Role::Archivist => matches!(self, Role::Owner | Role::Archivist),
            Role::Conductor => matches!(self, Role::Owner | Role::Conductor),
            Role::Musician => true,
        }
    }
}

/// Require the authenticated user to be `is_system_admin`. Used by instance-
/// level operations (CLAUDE.md: "create/delete Organizations, create/delete
/// Users"). Returns the user on success; `403 Forbidden` Problem Details
/// otherwise (not `404`, even though it also hides existence — per CLAUDE.md
/// the matrix violation itself, not resource existence, is the point).
pub fn require_system_admin(user: &User) -> Result<(), AppError> {
    if user.is_system_admin {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// Require the authenticated user to hold a `Membership` in `organization_id`
/// with a role satisfying `Role::at_least(minimum)`. `is_system_admin`
/// short-circuits to success regardless of membership (CLAUDE.md:
/// "`is_system_admin` short-circuits instance ops" — extended here to org
/// ops too, since an instance admin must be able to fix any org's broken
/// membership state, e.g. a wiped-out owner). Returns the caller's
/// `Membership` on success (handlers often need it, e.g. to forbid a
/// non-owner from editing their own role) or `403 Forbidden`.
pub async fn require_org_role(
    state: &AppState,
    user: &User,
    organization_id: Uuid,
    minimum: Role,
) -> Result<Option<membership::Membership>, AppError> {
    if user.is_system_admin {
        return Ok(None);
    }

    let found = membership::find_by_user_and_org(&state.db, user.id, organization_id)
        .await
        .map_err(AppError::from)?;

    match found {
        Some(m) if m.role.at_least(minimum) => Ok(Some(m)),
        _ => Err(AppError::Forbidden),
    }
}

/// Convenience wrapper over [`require_org_role`] for `/v1` handlers already
/// holding a [`BearerOrSession`].
pub async fn require_org_role_v1(
    state: &AppState,
    auth: &BearerOrSession,
    organization_id: Uuid,
    minimum: Role,
) -> Result<Option<membership::Membership>, AppError> {
    require_org_role(state, &auth.user, organization_id, minimum).await
}

/// Convenience wrapper over [`require_org_role`] for `/admin` handlers
/// already holding an [`AuthSession`].
pub async fn require_org_role_admin(
    state: &AppState,
    auth: &AuthSession,
    organization_id: Uuid,
    minimum: Role,
) -> Result<Option<membership::Membership>, AppError> {
    require_org_role(state, &auth.0, organization_id, minimum).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_meets_every_threshold() {
        assert!(Role::Owner.at_least(Role::Owner));
        assert!(Role::Owner.at_least(Role::Archivist));
        assert!(Role::Owner.at_least(Role::Conductor));
        assert!(Role::Owner.at_least(Role::Musician));
    }

    #[test]
    fn musician_only_meets_musician_threshold() {
        assert!(!Role::Musician.at_least(Role::Owner));
        assert!(!Role::Musician.at_least(Role::Archivist));
        assert!(!Role::Musician.at_least(Role::Conductor));
        assert!(Role::Musician.at_least(Role::Musician));
    }

    #[test]
    fn archivist_and_conductor_are_incomparable() {
        // Neither role satisfies the other's threshold — confirms this is
        // not a single total order, per the doc comment on `at_least`.
        assert!(!Role::Archivist.at_least(Role::Conductor));
        assert!(!Role::Conductor.at_least(Role::Archivist));
    }

    #[test]
    fn require_system_admin_rejects_non_admin() {
        let user = User {
            id: Uuid::now_v7(),
            slug: "alice".to_string(),
            username: "alice".to_string(),
            email: None,
            display_name: "Alice".to_string(),
            is_system_admin: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert!(matches!(
            require_system_admin(&user),
            Err(AppError::Forbidden)
        ));
    }

    #[test]
    fn require_system_admin_accepts_admin() {
        let user = User {
            id: Uuid::now_v7(),
            slug: "admin".to_string(),
            username: "admin".to_string(),
            email: None,
            display_name: "Admin".to_string(),
            is_system_admin: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert!(require_system_admin(&user).is_ok());
    }
}
