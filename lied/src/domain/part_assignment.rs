//! PartAssignment: which user plays which voice for a specific item in a
//! collection (CLAUDE.md Entities: PartAssignment). Issue #10 closes the
//! archivist→musician loop.
//!
//! **No soft-delete.** Unlike most entities, `part_assignment` has no
//! `deleted_at` (see `migrations/0014_part_assignment.sql`) — "unassign" is a
//! hard `DELETE`.
//!
//! **Reassignment replaces the row.** The unique index on
//! `(collection_item_id, voice_id)` means at most one assignee exists per
//! voice per item at any time. [`assign`] implements "replace" as an
//! `INSERT ... ON CONFLICT (collection_item_id, voice_id) DO UPDATE` — the row
//! keeps its original `id`, but `user_id` (and `notified_at`/`acknowledged_at`,
//! since a new assignee has been notified/acknowledged of nothing yet) are
//! overwritten. Reassigning to the *same* user is a no-op on the
//! notified/acknowledged state. `created_at`/`created_by` are **not** touched
//! on a replace: they record who first created the row and when, and are left
//! consistent with each other rather than pointing `created_by` at the last
//! re-assigner while `created_at` stays original (the actor of a reassignment
//! is captured in the audit log instead).
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
}

fn map_write_error(err: sqlx::Error) -> PartAssignmentError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.is_foreign_key_violation() {
            return PartAssignmentError::UnknownReference;
        }
    }
    PartAssignmentError::Database(err)
}

/// Assign `user_id` to `voice_id` on `collection_item_id`, or replace the
/// existing assignee if one already exists for that `(item, voice)` pair.
/// Returns the resulting row and whether it was a fresh insert (`true`) or a
/// replace of an existing row (`false`) — callers use this to pick `201` vs
/// `200`.
///
/// Enforces, atomically within the statement: `collection_item_id` must
/// reference a live item, and `voice_id` must reference a live voice under
/// that item's arrangement — else [`PartAssignmentError::VoiceNotInArrangement`]
/// (zero rows written, nothing to roll back).
pub async fn assign(
    pool: &PgPool,
    id: Uuid,
    collection_item_id: Uuid,
    voice_id: Uuid,
    user_id: Uuid,
    created_by: Option<Uuid>,
) -> Result<(PartAssignment, bool), PartAssignmentError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO part_assignment (id, collection_item_id, user_id, voice_id, created_by)
        SELECT $1, ci.id, $4, v.id, $5
        FROM collection_item ci
        JOIN voice v
          ON v.id = $3
         AND v.arrangement_id = ci.arrangement_id
         AND v.deleted_at IS NULL
        WHERE ci.id = $2 AND ci.deleted_at IS NULL
        ON CONFLICT (collection_item_id, voice_id) DO UPDATE SET
            user_id = excluded.user_id,
            notified_at = CASE
                WHEN part_assignment.user_id <> excluded.user_id THEN NULL
                ELSE part_assignment.notified_at
            END,
            acknowledged_at = CASE
                WHEN part_assignment.user_id <> excluded.user_id THEN NULL
                ELSE part_assignment.acknowledged_at
            END,
            updated_at = now()
        RETURNING
            id, collection_item_id, user_id, voice_id,
            notified_at as "notified_at: DateTime<Utc>",
            acknowledged_at as "acknowledged_at: DateTime<Utc>",
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            (xmax = 0) as "inserted!"
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

    let Some(row) = row else {
        return Err(PartAssignmentError::VoiceNotInArrangement);
    };

    Ok((
        PartAssignment {
            id: row.id,
            collection_item_id: row.collection_item_id,
            user_id: row.user_id,
            voice_id: row.voice_id,
            notified_at: row.notified_at,
            acknowledged_at: row.acknowledged_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            created_by: row.created_by,
        },
        row.inserted,
    ))
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

/// Look up the current assignment for a `(collection_item, voice)` pair, if
/// any. The assign endpoint uses this to enforce `If-Match` on a *replace*: a
/// PUT that overwrites an existing assignee carries the same optimistic-
/// concurrency contract as a PATCH/DELETE, so the caller must present the
/// current row's ETag.
pub async fn find_by_item_voice(
    pool: &PgPool,
    collection_item_id: Uuid,
    voice_id: Uuid,
) -> Result<Option<PartAssignment>, sqlx::Error> {
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
        "#,
        collection_item_id,
        voice_id,
    )
    .fetch_optional(pool)
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

/// Set `notified_at` / `acknowledged_at`. A `None` argument leaves that field
/// unchanged (this is a "set if provided" PATCH, not a way to clear a
/// timestamp back to `NULL` — clearing isn't a phase-1 need). Returns `None`
/// if no row matched.
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
pub async fn update_state(
    pool: &PgPool,
    id: Uuid,
    notified_at: Option<DateTime<Utc>>,
    acknowledged_at: Option<DateTime<Utc>>,
) -> Result<Option<PartAssignment>, sqlx::Error> {
    let notified_at_ms = notified_at.map(|d| d.timestamp_millis() as f64);
    let acknowledged_at_ms = acknowledged_at.map(|d| d.timestamp_millis() as f64);
    sqlx::query_as!(
        PartAssignment,
        r#"
        UPDATE part_assignment
        SET notified_at = COALESCE(to_timestamp($2::double precision / 1000.0), notified_at),
            acknowledged_at = COALESCE(to_timestamp($3::double precision / 1000.0), acknowledged_at),
            updated_at = now()
        WHERE id = $1
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
    )
    .fetch_optional(pool)
    .await
}

/// Hard-delete (unassign). No soft-delete on this entity — see the module
/// doc comment. Returns `true` if a row was removed.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(r#"DELETE FROM part_assignment WHERE id = $1"#, id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
