//! Who reads what of an organization's archive (docs/design.md, *Auth &
//! access*). Two rules live here, and only here:
//!
//! - **The read scope** ([`ReadScope`], [`read_scope`]): staff read the whole
//!   archive; a `musician` reads only their assigned parts, unless the org has
//!   opened the archive to musicians (`Organization.musicians_read_archive`,
//!   #119); everyone else (a guest, a non-member) reads only assigned parts.
//! - **What an assignment grants**: "a `PartAssignment` implicitly grants the
//!   assigned user read access to that voice's files for as long as the
//!   assignment exists, regardless of Membership in the org"
//!   ([`assigned_voice`], [`assigned_arrangement`]).
//!
//! The WebDAV tree ([`crate::webdav::access`]) and the `/v1` read paths
//! ([`crate::auth::authz::require_voice_reader_v1`] and
//! [`crate::auth::authz::require_arrangement_reader_v1`]) both call these, so
//! a musician or a guest sees the same voices over either surface, and
//! `GET /v1/me/parts` lists only parts these functions let the caller open.
//!
//! An assignment counts only while everything it hangs from is live: the
//! collection item and its collection, the voice and its arrangement.
//! Removing a piece from a collection, or deleting the collection, voice or
//! arrangement, ends the access without touching the assignment row.

use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::membership::Role;

/// How much of an organization's archive (arrangements, voices, full scores
/// and their files) a caller may **read**. Writes are a separate question,
/// decided by role ([`Role::can_edit_arrangements`] and friends): an
/// [`ReadScope::Archive`] musician still writes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadScope {
    /// `owner` / `archivist` / `conductor`: everything, licensing fields
    /// included.
    Staff,
    /// A `musician` in an org with `musicians_read_archive` on: every
    /// arrangement, voice and full score, read-only. Licensing and purchase
    /// fields stay the archive's business, as for [`ReadScope::Restricted`].
    Archive,
    /// A `musician` otherwise, or a guest or non-member: only what a live part
    /// assignment grants ([`assigned_voice`], [`assigned_arrangement`]).
    Restricted,
}

impl ReadScope {
    /// Whether this scope reads every arrangement, voice and score, assigned
    /// or not.
    pub fn reads_everything(self) -> bool {
        !matches!(self, ReadScope::Restricted)
    }
}

/// The read scope for a membership `role` (`None`: no membership) in an org
/// whose `musicians_read_archive` setting is `musicians_read_archive`. A pure
/// function, so the rule is testable without a database; [`read_scope`] feeds
/// it from one.
pub fn scope_for(role: Option<Role>, musicians_read_archive: bool) -> ReadScope {
    match role {
        Some(Role::Owner | Role::Archivist | Role::Conductor) => ReadScope::Staff,
        Some(Role::Musician) if musicians_read_archive => ReadScope::Archive,
        Some(Role::Musician) | None => ReadScope::Restricted,
    }
}

/// `user_id`'s read scope in organization `organization_id`, or `None` if the
/// organization does not exist. A system admin is not special here: callers
/// that let one in as owner-equivalent (`/v1`) check that first.
pub async fn read_scope(
    pool: &PgPool,
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Option<ReadScope>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT o.musicians_read_archive, m.role AS "role?"
           FROM organization o
           LEFT JOIN membership m ON m.organization_id = o.id AND m.user_id = $2
           WHERE o.id = $1"#,
        organization_id,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        // The `role` column is CHECK-constrained, so `parse` cannot fail for
        // real data; on drift, fall back to the least-privileged role.
        let role = r
            .role
            .map(|role| Role::parse(&role).unwrap_or(Role::Musician));
        scope_for(role, r.musicians_read_archive)
    }))
}

/// Where an assigned voice lives, for checking it against the request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignedVoice {
    pub organization_id: Uuid,
    pub arrangement_id: Uuid,
}

/// The voice `voice_id`, if `user_id` holds a live part assignment for it;
/// `None` otherwise (no assignment, or something it hangs from is deleted).
pub async fn assigned_voice(
    pool: &PgPool,
    user_id: Uuid,
    voice_id: Uuid,
) -> Result<Option<AssignedVoice>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT a.organization_id, a.id AS arrangement_id
           FROM voice v
           JOIN arrangement a ON a.id = v.arrangement_id
           WHERE v.id = $2
             AND v.deleted_at IS NULL
             AND a.deleted_at IS NULL
             AND EXISTS (
                 SELECT 1 FROM part_assignment pa
                 JOIN collection_item ci ON ci.id = pa.collection_item_id
                 JOIN collection c ON c.id = ci.collection_id
                 WHERE pa.user_id = $1 AND pa.voice_id = v.id
                   AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
             )"#,
        user_id,
        voice_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AssignedVoice {
        organization_id: r.organization_id,
        arrangement_id: r.arrangement_id,
    }))
}

/// The organization of arrangement `arrangement_id`, if `user_id` holds a
/// live part assignment on at least one of its voices; `None` otherwise.
/// Such a user may see the arrangement (its summary over `/v1`, its folder
/// over WebDAV) but only the voices [`assigned_voice`] grants.
pub async fn assigned_arrangement(
    pool: &PgPool,
    user_id: Uuid,
    arrangement_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT a.organization_id
           FROM arrangement a
           WHERE a.id = $2
             AND a.deleted_at IS NULL
             AND EXISTS (
                 SELECT 1 FROM part_assignment pa
                 JOIN voice v ON v.id = pa.voice_id
                 JOIN collection_item ci ON ci.id = pa.collection_item_id
                 JOIN collection c ON c.id = ci.collection_id
                 WHERE pa.user_id = $1
                   AND v.arrangement_id = a.id
                   AND v.deleted_at IS NULL
                   AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
             )"#,
        user_id,
        arrangement_id,
    )
    .fetch_optional(pool)
    .await
}

/// SQL condition, for the runtime-built list queries
/// (`voice::list_for_arrangement`, `arrangement::list_for_org`): the user bound
/// at `user_param` (`$5`, say) holds a live part assignment on the voice whose
/// id is `voice_id` (a column such as `v.id`). The same liveness as
/// [`assigned_voice`], whose compile-time-checked query cannot share a string
/// with it; the list tests open every listed entry through [`assigned_voice`]
/// and [`assigned_arrangement`], so the two cannot drift unnoticed. Both
/// arguments are fixed fragments from the caller, never input.
pub(crate) fn live_assignment_on_voice_sql(user_param: &str, voice_id: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM part_assignment pa_live
                 JOIN collection_item ci_live ON ci_live.id = pa_live.collection_item_id
                 JOIN collection c_live ON c_live.id = ci_live.collection_id
                 WHERE pa_live.user_id = {user_param} AND pa_live.voice_id = {voice_id}
                   AND ci_live.deleted_at IS NULL AND c_live.deleted_at IS NULL)"
    )
}

/// As [`live_assignment_on_voice_sql`], for an arrangement (a column such as
/// `a.id`): the user holds a live part assignment on one of its live voices,
/// [`assigned_arrangement`]'s condition.
pub(crate) fn live_assignment_on_arrangement_sql(user_param: &str, arrangement_id: &str) -> String {
    let on_voice = live_assignment_on_voice_sql(user_param, "v_live.id");
    format!(
        "EXISTS (SELECT 1 FROM voice v_live
                 WHERE v_live.arrangement_id = {arrangement_id} AND v_live.deleted_at IS NULL
                   AND {on_voice})"
    )
}

// ── the caller's parts (`GET /v1/me/parts`, #98) ─────────────────────────────

/// Sort allowlist for `GET /v1/me/parts`: wire name → SQL column. Every sort
/// is followed by organization name, collection name, position and id, so
/// pages are stable and the default reads as "per ensemble, per programme,
/// in running order". Unset times sort last in either direction, so
/// `acknowledgedAt:desc` starts with the latest acknowledgement.
pub const PARTS_SORT_ALLOWLIST: &[(&str, &str)] = &[
    ("organization", "o.name"),
    ("collection", "c.name"),
    ("position", "ci.position"),
    ("notifiedAt", "pa.notified_at"),
    ("acknowledgedAt", "pa.acknowledged_at"),
];

/// The default sort for `GET /v1/me/parts`: organization name ascending
/// (then collection name and position, as for every sort).
pub const PARTS_DEFAULT_SORT: (&str, crate::listing::SortDirection) =
    ("o.name", crate::listing::SortDirection::Asc);

/// Filter allowlist for `GET /v1/me/parts`.
pub const PARTS_FILTER_ALLOWLIST: &[&str] = &["organizationId", "collectionId", "acknowledged"];

/// Narrowing for [`list_parts`]; `None` means no filter.
#[derive(Debug, Clone, Copy, Default)]
pub struct PartFilters {
    pub organization_id: Option<Uuid>,
    pub collection_id: Option<Uuid>,
    /// `Some(true)`: only acknowledged parts; `Some(false)`: only the rest.
    pub acknowledged: Option<bool>,
}

/// One part the caller plays: a live assignment with everything a client
/// needs to show it and open it.
#[derive(Debug, Clone)]
pub struct Part {
    pub assignment_id: Uuid,
    pub organization_id: Uuid,
    pub organization_name: String,
    pub organization_slug: String,
    pub collection_id: Uuid,
    pub collection_name: String,
    pub collection_type: String,
    pub item_id: Uuid,
    pub item_position: i32,
    pub item_label: String,
    pub arrangement_id: Uuid,
    pub arrangement_title: String,
    pub voice_id: Uuid,
    pub voice_name: String,
    pub voice_instrument_ids: Vec<Uuid>,
    pub notified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub acknowledged_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A page of `user_id`'s live part assignments across every organization,
/// members' and guests' alike, and the total.
///
/// "Live" is exactly [`assigned_voice`]'s condition (collection item,
/// collection, voice and arrangement not deleted), and the organization is
/// the arrangement's, as there: every listed part opens through
/// `require_voice_reader_v1`. `sort_column` must come from
/// [`PARTS_SORT_ALLOWLIST`].
pub async fn list_parts(
    pool: &PgPool,
    user_id: Uuid,
    filters: PartFilters,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Part>, i64), sqlx::Error> {
    use sqlx::Row as _;
    let direction = sort_direction.as_sql();
    // Dynamic ORDER BY over allowlisted, fixed fragments only (see
    // `arrangement::list_for_org` for why this is injection-safe).
    let query = format!(
        r#"
        SELECT
            pa.id AS assignment_id, pa.notified_at, pa.acknowledged_at,
            o.id AS organization_id, o.name AS organization_name, o.slug AS organization_slug,
            c.id AS collection_id, c.name AS collection_name, c.type AS collection_type,
            ci.id AS item_id, ci.position AS item_position, ci.label AS item_label,
            a.id AS arrangement_id, a.title AS arrangement_title,
            v.id AS voice_id, v.name AS voice_name,
            ARRAY(SELECT vi.instrument_id FROM voice_instrument vi
                  WHERE vi.voice_id = v.id ORDER BY vi.position) AS voice_instrument_ids
        FROM part_assignment pa
        JOIN collection_item ci ON ci.id = pa.collection_item_id
        JOIN collection c ON c.id = ci.collection_id
        JOIN voice v ON v.id = pa.voice_id
        JOIN arrangement a ON a.id = v.arrangement_id
        JOIN organization o ON o.id = a.organization_id
        WHERE pa.user_id = $3
          AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
          AND v.deleted_at IS NULL AND a.deleted_at IS NULL
          AND ($4::uuid IS NULL OR o.id = $4)
          AND ($5::uuid IS NULL OR c.id = $5)
          AND ($6::bool IS NULL OR (pa.acknowledged_at IS NOT NULL) = $6)
        ORDER BY {sort_column} {direction} NULLS LAST, o.name, c.name, ci.position, pa.id
        LIMIT $1 OFFSET $2
        "#
    );
    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(user_id)
        .bind(filters.organization_id)
        .bind(filters.collection_id)
        .bind(filters.acknowledged)
        .fetch_all(pool)
        .await?;
    let parts = rows
        .into_iter()
        .map(|row| {
            Ok(Part {
                assignment_id: row.try_get("assignment_id")?,
                organization_id: row.try_get("organization_id")?,
                organization_name: row.try_get("organization_name")?,
                organization_slug: row.try_get("organization_slug")?,
                collection_id: row.try_get("collection_id")?,
                collection_name: row.try_get("collection_name")?,
                collection_type: row.try_get("collection_type")?,
                item_id: row.try_get("item_id")?,
                item_position: row.try_get("item_position")?,
                item_label: row.try_get("item_label")?,
                arrangement_id: row.try_get("arrangement_id")?,
                arrangement_title: row.try_get("arrangement_title")?,
                voice_id: row.try_get("voice_id")?,
                voice_name: row.try_get("voice_name")?,
                voice_instrument_ids: row.try_get("voice_instrument_ids")?,
                notified_at: row.try_get("notified_at")?,
                acknowledged_at: row.try_get("acknowledged_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!"
           FROM part_assignment pa
           JOIN collection_item ci ON ci.id = pa.collection_item_id
           JOIN collection c ON c.id = ci.collection_id
           JOIN voice v ON v.id = pa.voice_id
           JOIN arrangement a ON a.id = v.arrangement_id
           WHERE pa.user_id = $1
             AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
             AND v.deleted_at IS NULL AND a.deleted_at IS NULL
             AND ($2::uuid IS NULL OR a.organization_id = $2)
             AND ($3::uuid IS NULL OR c.id = $3)
             AND ($4::bool IS NULL OR (pa.acknowledged_at IS NOT NULL) = $4)"#,
        user_id,
        filters.organization_id,
        filters.collection_id,
        filters.acknowledged,
    )
    .fetch_one(pool)
    .await?;

    Ok((parts, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staff_read_everything_whatever_the_setting() {
        for role in [Role::Owner, Role::Archivist, Role::Conductor] {
            assert_eq!(scope_for(Some(role), false), ReadScope::Staff);
            assert_eq!(scope_for(Some(role), true), ReadScope::Staff);
        }
    }

    #[test]
    fn musicians_read_the_archive_only_when_the_org_opens_it() {
        assert_eq!(
            scope_for(Some(Role::Musician), false),
            ReadScope::Restricted
        );
        assert_eq!(scope_for(Some(Role::Musician), true), ReadScope::Archive);
        assert!(ReadScope::Archive.reads_everything());
        assert!(!ReadScope::Restricted.reads_everything());
    }

    #[test]
    fn guests_are_never_widened_by_the_setting() {
        assert_eq!(scope_for(None, false), ReadScope::Restricted);
        assert_eq!(scope_for(None, true), ReadScope::Restricted);
    }
}
