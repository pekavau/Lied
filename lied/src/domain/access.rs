//! Read access granted by a part assignment (docs/design.md, *Auth & access*:
//! "a `PartAssignment` implicitly grants the assigned user read access to
//! that voice's files for as long as the assignment exists, regardless of
//! Membership in the org").
//!
//! This is the one definition of that rule. The WebDAV tree
//! ([`crate::webdav::access`]) and the `/v1` read paths
//! ([`crate::auth::authz::require_voice_reader_v1`] and
//! [`crate::auth::authz::require_arrangement_reader_v1`]) both call it, so a
//! guest sees the same voices over either surface, and `GET /v1/me/parts`
//! lists only parts these functions let the caller open.
//!
//! An assignment counts only while everything it hangs from is live: the
//! collection item and its collection, the voice and its arrangement.
//! Removing a piece from a collection, or deleting the collection, voice or
//! arrangement, ends the access without touching the assignment row.

use sqlx::PgPool;
use uuid::Uuid;

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
