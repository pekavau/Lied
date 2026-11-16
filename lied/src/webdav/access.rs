//! Per-role visibility for the WebDAV arrangements tree (issue #8, step 2).
//!
//! Implements the read-filtering and PROPFIND root-scope rules from docs/design.md
//! ("WebDAV layout" → collections-subtree visibility + PROPFIND scope):
//!
//! - **Staff** (`owner` / `archivist` / `conductor`), and a `musician` of an
//!   org that opened its archive to musicians (`musicians_read_archive`,
//!   #119), see every arrangement in the org and the full score.
//! - **Restricted** — any other `musician` member, or a guest/substitute with
//!   *no* membership who reaches the tree via a `PartAssignment` — see only
//!   arrangements where they hold ≥1 part assignment, and never the full score.
//!
//! The scope itself is [`crate::domain::access::read_scope`], shared with
//! `/v1`. What may be *written* is decided by role, never by visibility.
//!
//! Soft-deleted rows are always hidden. These are the DB queries the
//! `DavFileSystem` layer calls from `read_dir`/`metadata`; keeping them here
//! makes the visibility policy unit-/integration-testable on its own.

use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::membership::Role;

/// How much of an org's arrangements tree a user may see: the shared
/// [`crate::domain::access::ReadScope`], so WebDAV and `/v1` cannot disagree.
pub use crate::domain::access::ReadScope as Visibility;

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
    Ok(crate::domain::access::read_scope(pool, org_id, user_id)
        .await?
        .map(|scope| (org_id, scope)))
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
    if visibility.reads_everything() {
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

/// Whether a restricted user may see a specific arrangement: holds a live
/// part assignment on one of its voices. Staff always may; callers should
/// short-circuit on [`Visibility::reads_everything`] before calling this. The rule is
/// [`crate::domain::access::assigned_arrangement`], shared with `/v1`.
pub async fn has_assignment_on_arrangement(
    pool: &PgPool,
    arrangement_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    Ok(
        crate::domain::access::assigned_arrangement(pool, user_id, arrangement_id)
            .await?
            .is_some(),
    )
}

/// Whether a user holds a live part assignment on a specific voice.
/// Restricted users may only see voices (and their files) they are assigned
/// to (docs/design.md: "musician sees only the files for voices where they
/// have a PartAssignment"). The rule is
/// [`crate::domain::access::assigned_voice`], shared with `/v1`.
pub async fn has_assignment_on_voice(
    pool: &PgPool,
    voice_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    Ok(
        crate::domain::access::assigned_voice(pool, user_id, voice_id)
            .await?
            .is_some(),
    )
}

// ── collections subtree (read-only computed view, issue #10) ───────────────
//
// The collections subtree is "not a separate permission domain" (docs/design.md):
// whatever a restricted user may see under `arrangements/` they may see here,
// scoped to *this specific collection item* rather than "any item ever". A
// restricted user (musician member, or guest with no membership) sees only
// collections/items they hold >=1 assignment on — mirroring the
// arrangements-root PROPFIND-scope rule (hide entirely, not an empty
// placeholder) — and within a visible item, only the voices they are
// assigned to on *that item*; the full score is always staff-only.

/// Whether a restricted user may see a collection at all — i.e. holds >=1
/// part assignment on one of its live items. Staff always may; callers
/// should short-circuit on [`Visibility::reads_everything`] before calling this.
pub async fn has_assignment_in_collection(
    pool: &PgPool,
    collection_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    // Only a *live* assignment target counts: the item, its arrangement, and
    // the assigned voice must all be un-deleted. Otherwise a guest whose sole
    // assignment sits on a since-soft-deleted arrangement/voice would see the
    // collection listed but find it empty on entry (the item/voice listings
    // filter these out) — the "phantom empty directory" the module doc forbids.
    // Kept in lockstep with `visible_collection_items` and
    // `has_assignment_on_item`.
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM part_assignment pa
             JOIN collection_item ci ON ci.id = pa.collection_item_id
             JOIN arrangement a ON a.id = ci.arrangement_id
             JOIN voice v ON v.id = pa.voice_id
             WHERE ci.collection_id = $1 AND pa.user_id = $2
               AND ci.deleted_at IS NULL
               AND a.deleted_at IS NULL
               AND v.deleted_at IS NULL
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
    // Requires the assignment to target a live voice (and live item/arrangement),
    // matching `visible_item_voices` — otherwise the item directory would be
    // visible but enumerate no voices and no (non-staff) score.
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM part_assignment pa
             JOIN collection_item ci ON ci.id = pa.collection_item_id
             JOIN arrangement a ON a.id = ci.arrangement_id
             JOIN voice v ON v.id = pa.voice_id
             WHERE pa.collection_item_id = $1 AND pa.user_id = $2
               AND ci.deleted_at IS NULL
               AND a.deleted_at IS NULL
               AND v.deleted_at IS NULL
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
    if visibility.reads_everything() {
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
               JOIN arrangement a ON a.id = ci.arrangement_id
               JOIN part_assignment pa ON pa.collection_item_id = ci.id
               JOIN voice v ON v.id = pa.voice_id
               WHERE c.organization_id = $1 AND c.deleted_at IS NULL
                 AND ci.deleted_at IS NULL
                 AND a.deleted_at IS NULL
                 AND v.deleted_at IS NULL
                 AND pa.user_id = $2
               ORDER BY c.slug"#,
            org_id,
            user_id,
        )
        .fetch_all(pool)
        .await
    }
}

/// A live collection item as seen through the WebDAV collections tree: its id,
/// label (with the collection's number width), and its arrangement's slug (the item's
/// WebDAV directory name is [`VisibleCollectionItem::folder_name`]). An item whose arrangement is soft-deleted is
/// excluded — unlike the REST hide-with-references view (docs/design.md
/// `CollectionItemView::arrangement_removed`), there is no meaningful
/// directory to show for a filesystem view with no live files behind it.
pub struct VisibleCollectionItem {
    pub id: Uuid,
    pub label: String,
    /// The collection's `number_width`, which the folder name is padded to.
    pub number_width: i16,
    pub arrangement_slug: String,
}

impl VisibleCollectionItem {
    /// The item's WebDAV directory name: `<padded label>-<arrangement slug>`.
    pub fn folder_name(&self) -> String {
        crate::domain::collection_item::folder_name(
            &self.label,
            self.number_width,
            &self.arrangement_slug,
        )
    }
}

/// Live items in a collection, in position order, visible to the user: staff see
/// all; a restricted user sees only items they hold >=1 assignment on.
pub async fn visible_collection_items(
    pool: &PgPool,
    collection_id: Uuid,
    user_id: Uuid,
    visibility: Visibility,
) -> Result<Vec<VisibleCollectionItem>, sqlx::Error> {
    if visibility.reads_everything() {
        let rows = sqlx::query!(
            r#"SELECT ci.id, ci.label, c.number_width, a.slug as arrangement_slug
               FROM collection_item ci
               JOIN collection c ON c.id = ci.collection_id
               JOIN arrangement a ON a.id = ci.arrangement_id
               WHERE ci.collection_id = $1 AND ci.deleted_at IS NULL AND a.deleted_at IS NULL
               ORDER BY ci.position ASC, ci.id ASC"#,
            collection_id,
        )
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| VisibleCollectionItem {
                id: r.id,
                label: r.label,
                number_width: r.number_width,
                arrangement_slug: r.arrangement_slug,
            })
            .collect())
    } else {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT ci.id, ci.position, ci.label, c.number_width, a.slug as arrangement_slug
               FROM collection_item ci
               JOIN collection c ON c.id = ci.collection_id
               JOIN arrangement a ON a.id = ci.arrangement_id
               JOIN part_assignment pa ON pa.collection_item_id = ci.id
               JOIN voice v ON v.id = pa.voice_id
               WHERE ci.collection_id = $1 AND ci.deleted_at IS NULL AND a.deleted_at IS NULL
                 AND v.deleted_at IS NULL
                 AND pa.user_id = $2
               ORDER BY ci.position ASC, ci.id ASC"#,
            collection_id,
            user_id,
        )
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| VisibleCollectionItem {
                id: r.id,
                label: r.label,
                number_width: r.number_width,
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
