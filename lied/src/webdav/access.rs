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
                     JOIN collection_item ci ON ci.id = pa.collection_item_id
                     JOIN collection c ON c.id = ci.collection_id
                     WHERE pa.user_id = $2
                       AND v.arrangement_id = a.id
                       AND v.deleted_at IS NULL
                       AND ci.deleted_at IS NULL
                       AND c.deleted_at IS NULL
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
             JOIN collection_item ci ON ci.id = pa.collection_item_id
             JOIN collection c ON c.id = ci.collection_id
             WHERE pa.user_id = $1
               AND v.arrangement_id = $2
               AND v.deleted_at IS NULL
               AND ci.deleted_at IS NULL
               AND c.deleted_at IS NULL
           ) AS "exists!""#,
        user_id,
        arrangement_id,
    )
    .fetch_one(pool)
    .await
}

/// Whether a user holds a part assignment on a specific voice. Restricted
/// users may only see voices (and their files) they are assigned to
/// (CLAUDE.md: "musician sees only the files for voices where they have a
/// PartAssignment").
pub async fn has_assignment_on_voice(
    pool: &PgPool,
    voice_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM part_assignment pa
             JOIN collection_item ci ON ci.id = pa.collection_item_id
             JOIN collection c ON c.id = ci.collection_id
             WHERE pa.user_id = $1 AND pa.voice_id = $2
               AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
           ) AS "exists!""#,
        user_id,
        voice_id,
    )
    .fetch_one(pool)
    .await
}

// ── collections subtree (read-only computed view, issue #10) ───────────────
//
// The collections subtree is "not a separate permission domain" (CLAUDE.md):
// whatever a restricted user may see under `arrangements/` they may see here,
// scoped to *this specific collection item* rather than "any item ever". A
// restricted user (musician member, or guest with no membership) sees only
// collections/items they hold >=1 assignment on — mirroring the
// arrangements-root PROPFIND-scope rule (hide entirely, not an empty
// placeholder) — and within a visible item, only the voices they are
// assigned to on *that item*; the full score is always staff-only.

/// Whether a restricted user may see a collection at all — i.e. holds >=1
/// part assignment on one of its live items. Staff always may; callers
/// should short-circuit on [`Visibility::is_staff`] before calling this.
pub async fn has_assignment_in_collection(
    pool: &PgPool,
    collection_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM part_assignment pa
             JOIN collection_item ci ON ci.id = pa.collection_item_id
             WHERE ci.collection_id = $1 AND pa.user_id = $2 AND ci.deleted_at IS NULL
           ) AS "exists!""#,
        collection_id,
        user_id,
    )
    .fetch_one(pool)
    .await
}

/// Whether a restricted user may see a specific collection item — i.e. holds
/// >=1 part assignment on it (any voice). Staff always may.
pub async fn has_assignment_on_item(
    pool: &PgPool,
    collection_item_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM part_assignment pa
             WHERE pa.collection_item_id = $1 AND pa.user_id = $2
           ) AS "exists!""#,
        collection_item_id,
        user_id,
    )
    .fetch_one(pool)
    .await
}

/// Whether a user holds a part assignment on `voice_id` **specifically for
/// `collection_item_id`** — stricter than [`has_assignment_on_voice`], which
/// matches the voice on *any* item. The collections-subtree view scopes
/// visibility to one concert program's item, not "this voice, on whichever
/// item it was ever assigned".
pub async fn has_assignment_on_item_voice(
    pool: &PgPool,
    collection_item_id: Uuid,
    voice_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM part_assignment pa
             WHERE pa.collection_item_id = $1 AND pa.voice_id = $2 AND pa.user_id = $3
           ) AS "exists!""#,
        collection_item_id,
        voice_id,
        user_id,
    )
    .fetch_one(pool)
    .await
}

/// Collection slugs visible to a user in an org: staff see every live
/// collection; a restricted user sees only collections containing >=1 item
/// they hold an assignment on. Ordered by slug.
pub async fn visible_collection_slugs(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    visibility: Visibility,
) -> Result<Vec<String>, sqlx::Error> {
    if visibility.is_staff() {
        sqlx::query_scalar!(
            r#"SELECT slug FROM collection
               WHERE organization_id = $1 AND deleted_at IS NULL
               ORDER BY slug"#,
            org_id,
        )
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_scalar!(
            r#"SELECT DISTINCT c.slug FROM collection c
               JOIN collection_item ci ON ci.collection_id = c.id
               JOIN part_assignment pa ON pa.collection_item_id = ci.id
               WHERE c.organization_id = $1 AND c.deleted_at IS NULL
                 AND ci.deleted_at IS NULL AND pa.user_id = $2
               ORDER BY c.slug"#,
            org_id,
            user_id,
        )
        .fetch_all(pool)
        .await
    }
}

/// A live collection item as seen through the WebDAV collections tree: its id,
/// index, and its arrangement's slug (the item's WebDAV directory name is
/// `<index>-<arrangementSlug>`). An item whose arrangement is soft-deleted is
/// excluded — unlike the REST hide-with-references view (CLAUDE.md
/// `CollectionItemView::arrangement_removed`), there is no meaningful
/// directory to show for a filesystem view with no live files behind it.
pub struct VisibleCollectionItem {
    pub id: Uuid,
    pub index: i32,
    pub arrangement_slug: String,
}

/// Live items in a collection, in index order, visible to the user: staff see
/// all; a restricted user sees only items they hold >=1 assignment on.
pub async fn visible_collection_items(
    pool: &PgPool,
    collection_id: Uuid,
    user_id: Uuid,
    visibility: Visibility,
) -> Result<Vec<VisibleCollectionItem>, sqlx::Error> {
    if visibility.is_staff() {
        let rows = sqlx::query!(
            r#"SELECT ci.id, ci.index, a.slug as arrangement_slug
               FROM collection_item ci
               JOIN arrangement a ON a.id = ci.arrangement_id
               WHERE ci.collection_id = $1 AND ci.deleted_at IS NULL AND a.deleted_at IS NULL
               ORDER BY ci.index ASC, ci.id ASC"#,
            collection_id,
        )
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| VisibleCollectionItem {
                id: r.id,
                index: r.index,
                arrangement_slug: r.arrangement_slug,
            })
            .collect())
    } else {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT ci.id, ci.index, a.slug as arrangement_slug
               FROM collection_item ci
               JOIN arrangement a ON a.id = ci.arrangement_id
               JOIN part_assignment pa ON pa.collection_item_id = ci.id
               WHERE ci.collection_id = $1 AND ci.deleted_at IS NULL AND a.deleted_at IS NULL
                 AND pa.user_id = $2
               ORDER BY ci.index ASC, ci.id ASC"#,
            collection_id,
            user_id,
        )
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| VisibleCollectionItem {
                id: r.id,
                index: r.index,
                arrangement_slug: r.arrangement_slug,
            })
            .collect())
    }
}

/// Org slugs a user has any relationship with — a membership, or a part
/// assignment (guest/substitute). Used to scope the `/orgs` root listing so it
/// does not enumerate every org on the instance. Ordered by slug.
pub async fn visible_org_slugs(pool: &PgPool, user_id: Uuid) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT o.slug FROM organization o
        WHERE EXISTS (
            SELECT 1 FROM membership m
            WHERE m.organization_id = o.id AND m.user_id = $1
        )
        OR EXISTS (
            SELECT 1 FROM part_assignment pa
            JOIN voice v ON v.id = pa.voice_id
            JOIN arrangement a ON a.id = v.arrangement_id
            JOIN collection_item ci ON ci.id = pa.collection_item_id
            JOIN collection c ON c.id = ci.collection_id
            WHERE pa.user_id = $1 AND a.organization_id = o.id
              AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
        )
        ORDER BY o.slug
        "#,
        user_id,
    )
    .fetch_all(pool)
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
