//! Per-role visibility for the WebDAV arrangements tree (issue #8, step 2).
//!
//! Implements the read-filtering and PROPFIND root-scope rules from CLAUDE.md
//! ("WebDAV layout" → collections-subtree visibility + PROPFIND scope):
//!
//! - **Staff** (`owner` / `archivist` / `conductor`) see every arrangement in
//!   the org and the full score.
//! - **Restricted** — a `musician` member, or a guest/substitute with *no*
//!   membership who reaches the tree via a `PartAssignment` — see only
//!   arrangements where they hold ≥1 part assignment, and never the full score.
//!
//! Soft-deleted rows are always hidden. These are the DB queries the
//! `DavFileSystem` layer calls from `read_dir`/`metadata`; keeping them here
//! makes the visibility policy unit-/integration-testable on its own.

use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::membership::Role;

/// How much of an org's arrangements tree a user may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// `owner` / `archivist` / `conductor`: all arrangements + full scores.
    Staff,
    /// `musician` member or guest (assignment-only): assigned arrangements,
    /// full score hidden.
    Restricted,
}

impl Visibility {
    pub fn is_staff(self) -> bool {
        matches!(self, Visibility::Staff)
    }
}

/// Classify a membership role (or its absence) into a [`Visibility`]. `None`
/// means no membership in the org — a guest/substitute reaching the tree via a
/// part assignment — which is restricted.
pub fn visibility_for(role: Option<Role>) -> Visibility {
    match role {
        Some(Role::Owner | Role::Archivist | Role::Conductor) => Visibility::Staff,
        Some(Role::Musician) | None => Visibility::Restricted,
    }
}

/// Resolve an org slug to its id. Organizations have no soft-delete.
pub async fn find_org_id(pool: &PgPool, org_slug: &str) -> Result<Option<Uuid>, sqlx::Error> {
    let row = sqlx::query!(r#"SELECT id FROM organization WHERE slug = $1"#, org_slug)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.id))
}

/// The user's membership role in the org, or `None` if they are not a member
/// (a guest/substitute reaching the tree through a part assignment).
pub async fn role_in_org(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<Option<Role>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT role FROM membership WHERE organization_id = $1 AND user_id = $2"#,
        org_id,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    // The `role` column is CHECK-constrained, so `parse` cannot fail for real
    // data; default to the least-privileged role on unexpected drift rather
    // than erroring the whole WebDAV request.
    Ok(row.map(|r| Role::parse(&r.role).unwrap_or(Role::Musician)))
}

/// Resolve `(org_id, visibility)` for a user in one step. `None` if the org
/// slug does not exist.
pub async fn resolve_org_visibility(
    pool: &PgPool,
    org_slug: &str,
    user_id: Uuid,
) -> Result<Option<(Uuid, Visibility)>, sqlx::Error> {
    let Some(org_id) = find_org_id(pool, org_slug).await? else {
        return Ok(None);
    };
    let role = role_in_org(pool, org_id, user_id).await?;
    Ok(Some((org_id, visibility_for(role))))
}

/// The arrangement slugs a user may see at the arrangements root, honoring the
/// PROPFIND root-scope rule: staff see all live arrangements; a restricted user
/// sees only those where they hold ≥1 part assignment on a live voice.
/// Soft-deleted arrangements are always excluded. Ordered by slug.
pub async fn visible_arrangement_slugs(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    visibility: Visibility,
) -> Result<Vec<String>, sqlx::Error> {
    if visibility.is_staff() {
        sqlx::query_scalar!(
            r#"SELECT slug FROM arrangement
               WHERE organization_id = $1 AND deleted_at IS NULL
               ORDER BY slug"#,
            org_id,
        )
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_scalar!(
            r#"SELECT DISTINCT a.slug FROM arrangement a
               WHERE a.organization_id = $1 AND a.deleted_at IS NULL
                 AND EXISTS (
                     SELECT 1 FROM part_assignment pa
                     JOIN voice v ON v.id = pa.voice_id
                     WHERE pa.user_id = $2
                       AND v.arrangement_id = a.id
                       AND v.deleted_at IS NULL
                 )
               ORDER BY a.slug"#,
            org_id,
            user_id,
        )
        .fetch_all(pool)
        .await
    }
}

/// Whether a restricted user may see a specific arrangement — i.e. holds ≥1
/// part assignment on one of its live voices. Staff always may; callers should
/// short-circuit on [`Visibility::is_staff`] before calling this.
pub async fn has_assignment_on_arrangement(
    pool: &PgPool,
    arrangement_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM part_assignment pa
             JOIN voice v ON v.id = pa.voice_id
             WHERE pa.user_id = $1
               AND v.arrangement_id = $2
               AND v.deleted_at IS NULL
           ) AS "exists!""#,
        user_id,
        arrangement_id,
    )
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staff_roles_map_to_staff_visibility() {
        assert!(visibility_for(Some(Role::Owner)).is_staff());
        assert!(visibility_for(Some(Role::Archivist)).is_staff());
        assert!(visibility_for(Some(Role::Conductor)).is_staff());
    }

    #[test]
    fn musician_and_guest_are_restricted() {
        assert!(!visibility_for(Some(Role::Musician)).is_staff());
        // No membership at all (guest/substitute) → restricted.
        assert!(!visibility_for(None).is_staff());
    }
}
