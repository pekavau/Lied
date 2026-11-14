//! PartAssignment: which user plays which voice for a specific item in a
//! collection (CLAUDE.md Entities: PartAssignment). Issue #10 closes the
//! archivist→musician loop.
//!
//! **No soft-delete.** Unlike most entities, `part_assignment` has no
//! `deleted_at` (see `migrations/0014_part_assignment.sql`) — "unassign" is a
//! hard `DELETE`.
//!
//! **Several players per part.** The unique index is on
//! `(collection_item_id, voice_id, user_id)` (issue #53): a part is played by as
//! many musicians as the section has, so [`assign`] adds a player and never
//! replaces one; only the same person twice on one part is refused. Moving a
//! part to someone else is remove-then-add, each audited. Assigning one player
//! to the same part across a whole collection is
//! [`crate::domain::bulk_assign`], which writes ordinary rows of this table.
//!
//! **Implicit read grant.** A `PartAssignment` does not require the assigned
//! user to hold a `Membership` in the org — this is how guest/substitute
//! musicians are modeled (CLAUDE.md: "grants the assigned user read access to
//! that voice's files for as long as the assignment exists"). The WebDAV
//! collections-subtree visibility (`webdav::access`, `webdav::fs`) is what
//! actually enforces that grant; this module only owns the row.
//!
//! **Cross-entity validation Postgres can't express.** `voice_id` must
//! reference a voice belonging to the *same arrangement* as the
//! `collection_item`'s arrangement — there is no FK for that. [`assign`]
//! enforces it atomically as part of the same statement (a `WHERE EXISTS`
//! guard on the `INSERT ... SELECT`), so a mismatched voice results in zero
//! rows written rather than a race between a separate check and the write.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of a `part_assignment` row.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PartAssignment {
    pub id: Uuid,
    pub collection_item_id: Uuid,
    pub user_id: Uuid,
    pub voice_id: Uuid,
    pub notified_at: Option<DateTime<Utc>>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
}

#[derive(thiserror::Error, Debug)]
pub enum PartAssignmentError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    /// The voice does not exist (live) under the same arrangement as the
    /// collection item, so it cannot be assigned there.
    #[error("the voice does not belong to this collection item's arrangement")]
    VoiceNotInArrangement,
    /// A referenced row (e.g. `user_id`) does not exist — surfaced as a plain
    /// FK violation rather than the more specific check above.
    #[error("a referenced entity does not exist")]
    UnknownReference,
    /// This person already plays this voice on this piece. Several musicians
    /// per part is the point of the model; the same one twice is not.
    #[error("this musician is already assigned to this voice")]
    Duplicate,
}

fn map_write_error(err: sqlx::Error) -> PartAssignmentError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("part_assignment_collection_item_id_voice_id_user_id_key") {
            return PartAssignmentError::Duplicate;
        }
        if db_err.is_foreign_key_violation() {
            return PartAssignmentError::UnknownReference;
        }
    }
    PartAssignmentError::Database(err)
}

/// Add `user_id` as a player of `voice_id` on `collection_item_id`.
///
/// **Adds; never replaces.** A part is played by as many musicians as the
/// section has, so a second assignment is a second player, not a correction of
/// the first (issue #53). The same person twice on one part is still a mistake
/// and maps to [`PartAssignmentError::Duplicate`].
///
/// Enforces, atomically within the statement: `collection_item_id` must
/// reference a live item, and `voice_id` a live voice under that item's
/// arrangement — else [`PartAssignmentError::VoiceNotInArrangement`] with zero
/// rows written.
pub async fn assign(
    pool: &PgPool,
    id: Uuid,
    collection_item_id: Uuid,
    voice_id: Uuid,
    user_id: Uuid,
    created_by: Option<Uuid>,
) -> Result<PartAssignment, PartAssignmentError> {
    let row = sqlx::query_as!(
        PartAssignment,
        r#"
        INSERT INTO part_assignment (id, collection_item_id, user_id, voice_id, created_by)
        SELECT $1, ci.id, $4, v.id, $5
        FROM collection_item ci
        JOIN voice v
          ON v.id = $3
         AND v.arrangement_id = ci.arrangement_id
         AND v.deleted_at IS NULL
        WHERE ci.id = $2 AND ci.deleted_at IS NULL
        RETURNING
            id, collection_item_id, user_id, voice_id,
            notified_at as "notified_at: DateTime<Utc>",
            acknowledged_at as "acknowledged_at: DateTime<Utc>",
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by
        "#,
        id,
        collection_item_id,
        voice_id,
        user_id,
        created_by,
    )
    .fetch_optional(pool)
    .await
    .map_err(map_write_error)?;

    // No row means the INSERT..SELECT matched nothing: the item or the voice is
    // gone, or the voice belongs to a different arrangement.
    row.ok_or(PartAssignmentError::VoiceNotInArrangement)
}

/// Look up an assignment by id.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<PartAssignment>, sqlx::Error> {
    sqlx::query_as!(
        PartAssignment,
        r#"
        SELECT
            id, collection_item_id, user_id, voice_id,
            notified_at as "notified_at: DateTime<Utc>",
            acknowledged_at as "acknowledged_at: DateTime<Utc>",
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by
        FROM part_assignment
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Every musician playing a `(collection_item, voice)` pair, oldest assignment
/// first. A section holds as many as it has players; an empty result is an
/// uncovered part.
pub async fn list_for_item_voice(
    pool: &PgPool,
    collection_item_id: Uuid,
    voice_id: Uuid,
) -> Result<Vec<PartAssignment>, sqlx::Error> {
    sqlx::query_as!(
        PartAssignment,
        r#"
        SELECT
            id, collection_item_id, user_id, voice_id,
            notified_at as "notified_at: DateTime<Utc>",
            acknowledged_at as "acknowledged_at: DateTime<Utc>",
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by
        FROM part_assignment
        WHERE collection_item_id = $1 AND voice_id = $2
        ORDER BY created_at ASC, id ASC
        "#,
        collection_item_id,
        voice_id,
    )
    .fetch_all(pool)
    .await
}

/// One page of a collection item's assignments, newest first.
pub async fn list_for_item(
    pool: &PgPool,
    collection_item_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PartAssignment>, i64), sqlx::Error> {
    let rows = sqlx::query_as!(
        PartAssignment,
        r#"
        SELECT
            id, collection_item_id, user_id, voice_id,
            notified_at as "notified_at: DateTime<Utc>",
            acknowledged_at as "acknowledged_at: DateTime<Utc>",
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by
        FROM part_assignment
        WHERE collection_item_id = $1
        ORDER BY created_at DESC, id ASC
        LIMIT $2 OFFSET $3
        "#,
        collection_item_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"SELECT count(*) as "count!" FROM part_assignment WHERE collection_item_id = $1"#,
        collection_item_id,
    )
    .fetch_one(pool)
    .await?;

    Ok((rows, total))
}

/// What an update does to one distribution timestamp.
pub type StampChange = super::Change<DateTime<Utc>>;

/// Set or clear `notified_at` / `acknowledged_at`; [`StampChange::Keep`]
/// leaves that field unchanged. Returns `None` if no row matched.
///
/// Timestamps are passed through as millisecond-epoch `i64` (via
/// `to_timestamp(.../1000.0)`) rather than binding `DateTime<Utc>` directly:
/// this workspace links both `chrono` and `time` into `sqlx` (the latter
/// pulled in transitively by `tower-sessions-sqlx-store`), which makes the
/// query macros' compile-time parameter-type check ambiguous for a bound
/// timestamp (it infers `time::OffsetDateTime`, not `chrono::DateTime<Utc>`,
/// with no per-parameter override syntax available — unlike `RETURNING`
/// columns, which use `as "col: DateTime<Utc>"`). Binding a plain `i64` sidesteps
/// the ambiguity entirely; every other write in this codebase avoids the
/// conflict the other way, by using server-side `now()` instead of a bound
/// timestamp.
///
/// `expected_version` is the caller's ETag (millisecond `updated_at`). With
/// `Some`, the write happens only if the row is still at that version, checked
/// inside the statement so two writers holding one ETag cannot both succeed;
/// otherwise nothing is written and this reports no row (`None` / `false`),
/// exactly as for a missing one — the caller tells the two apart. `None`
/// writes unconditionally (fixtures, internal callers with no ETag).
pub async fn update_state(
    pool: &PgPool,
    id: Uuid,
    expected_version: Option<i64>,
    notified_at: StampChange,
    acknowledged_at: StampChange,
) -> Result<Option<PartAssignment>, sqlx::Error> {
    fn bind(change: StampChange) -> (bool, Option<f64>) {
        match change {
            StampChange::Keep => (true, None),
            StampChange::Set(value) => (false, value.map(|d| d.timestamp_millis() as f64)),
        }
    }
    let (keep_notified, notified_at_ms) = bind(notified_at);
    let (keep_acknowledged, acknowledged_at_ms) = bind(acknowledged_at);
    sqlx::query_as!(
        PartAssignment,
        r#"
        UPDATE part_assignment
        SET notified_at = CASE WHEN $4 THEN notified_at
                               ELSE to_timestamp($2::double precision / 1000.0) END,
            acknowledged_at = CASE WHEN $5 THEN acknowledged_at
                                   ELSE to_timestamp($3::double precision / 1000.0) END,
            updated_at = now()
        WHERE id = $1 AND ($6::bigint IS NULL OR floor(extract(epoch FROM updated_at) * 1000)::bigint = $6)
        RETURNING
            id, collection_item_id, user_id, voice_id,
            notified_at as "notified_at: DateTime<Utc>",
            acknowledged_at as "acknowledged_at: DateTime<Utc>",
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by
        "#,
        id,
        notified_at_ms,
        acknowledged_at_ms,
        keep_notified,
        keep_acknowledged,
        expected_version,
    )
    .fetch_optional(pool)
    .await
}

/// Hard-delete (unassign). No soft-delete on this entity — see the module
/// doc comment. Returns `true` if a row was removed.
///
/// `expected_version` is the caller's ETag (millisecond `updated_at`). With
/// `Some`, the write happens only if the row is still at that version, checked
/// inside the statement so two writers holding one ETag cannot both succeed;
/// otherwise nothing is written and this reports no row (`None` / `false`),
/// exactly as for a missing one — the caller tells the two apart. `None`
/// writes unconditionally (fixtures, internal callers with no ETag).
pub async fn delete(
    pool: &PgPool,
    id: Uuid,
    expected_version: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"DELETE FROM part_assignment WHERE id = $1 AND ($2::bigint IS NULL OR floor(extract(epoch FROM updated_at) * 1000)::bigint = $2)"#,
        id,
        expected_version,
    )
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
