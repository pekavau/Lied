//! CollectionItem: an arrangement within a collection at a local index number
//! (CLAUDE.md Entities: CollectionItem). Issue #9.
//!
//! **Hide-with-references.** An item whose arrangement has been soft-deleted is
//! *not* dropped from the collection — it is surfaced as "removed" (the
//! [`CollectionItemView::arrangement_removed`] flag) with the arrangement slug
//! still shown, so a broken reference is visible rather than silently missing
//! (CLAUDE.md soft-delete cascade).
//!
//! **Unique index.** `(collection_id, index)` is unique among live rows; a
//! clash maps to [`CollectionItemError::DuplicateIndex`]. [`reorder`] reassigns
//! a whole collection's indices atomically.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of a `collection_item` row.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItem {
    pub id: Uuid,
    pub collection_id: Uuid,
    pub arrangement_id: Uuid,
    pub index: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// A collection item plus its referenced arrangement's display info, so a
/// soft-deleted ("removed") arrangement is surfaced rather than dropped.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItemView {
    #[serde(flatten)]
    pub item: CollectionItem,
    pub arrangement_slug: String,
    pub arrangement_title: String,
    /// `true` when the referenced arrangement is soft-deleted — render it as
    /// "[removed]" with the slug, not as a live entry.
    pub arrangement_removed: bool,
}

/// Highest allowed piece number.
///
/// [`reorder`] parks every index at `index + INDEX_PARK_OFFSET` to clear the
/// unique `(collection_id, index)` space before renumbering, so an index near
/// `i32::MAX` would overflow Postgres `integer` mid-transaction. Capping well
/// below that keeps the arithmetic safe, and a collection with a million pieces
/// is not a thing.
pub const MAX_INDEX: i32 = 999_999;

/// The offset [`reorder`] parks indices at. Must exceed any legal index so the
/// parked values cannot collide with un-parked ones.
const INDEX_PARK_OFFSET: i32 = 1_000_000;

/// Whether `index` is a usable piece number: positive and within [`MAX_INDEX`].
/// Both write surfaces validate through this — the console form and `/v1` had
/// drifted, and the console's missing cap turned into a 500 on the next
/// reorder.
pub fn is_valid_index(index: i32) -> bool {
    (1..=MAX_INDEX).contains(&index)
}

#[derive(thiserror::Error, Debug)]
pub enum CollectionItemError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("an item already exists at this index in the collection")]
    DuplicateIndex,
    #[error("the referenced collection or arrangement does not exist")]
    UnknownReference,
    #[error("the item order must be exactly the collection's current live items")]
    InvalidReorder,
    /// This arrangement is already a live piece in the collection. Adding it
    /// twice — or restoring a removed piece after re-adding the same
    /// arrangement — would list the same music under two numbers.
    #[error("this arrangement is already in the collection")]
    DuplicateArrangement,
}

fn map_write_error(err: sqlx::Error) -> CollectionItemError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("collection_item_collection_id_index_key") {
            return CollectionItemError::DuplicateIndex;
        }
        if db_err.constraint() == Some("collection_item_collection_id_arrangement_id_key") {
            return CollectionItemError::DuplicateArrangement;
        }
        if db_err.is_foreign_key_violation() {
            return CollectionItemError::UnknownReference;
        }
    }
    CollectionItemError::Database(err)
}

/// Add an arrangement to a collection at `index`.
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    collection_id: Uuid,
    arrangement_id: Uuid,
    index: i32,
    created_by: Option<Uuid>,
) -> Result<CollectionItem, CollectionItemError> {
    let row = sqlx::query_as!(
        CollectionItem,
        r#"
        INSERT INTO collection_item (id, collection_id, arrangement_id, index, created_by)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING
            id, collection_id, arrangement_id, index,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        collection_id,
        arrangement_id,
        index,
        created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row)
}

/// Look up a live item by id (soft-deleted hidden).
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<CollectionItem>, sqlx::Error> {
    sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT
            id, collection_id, arrangement_id, index,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection_item
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Look up an item by id including soft-deleted rows (undelete flow).
pub async fn find_by_id_including_deleted(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<CollectionItem>, sqlx::Error> {
    sqlx::query_as!(
        CollectionItem,
        r#"
        SELECT
            id, collection_id, arrangement_id, index,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection_item
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// List a collection's live items in index order, each with its arrangement's
/// display info (surfacing soft-deleted arrangements as removed). Items are
/// hidden if the collection itself is soft-deleted.
///
/// **Deliberately unpaginated.** Every caller needs the whole set to be
/// correct, not merely complete: the console renders one reorder form over all
/// rows and submits their ids as the new order, and the domain's [`reorder`]
/// rejects anything that is not a permutation of the live items — so a page of
/// them would be refused by construction. The bound is the collection itself: a
/// program is a concert and a standing collection a book, both of which are
/// tens of entries, not thousands.
pub async fn list_for_collection(
    pool: &PgPool,
    collection_id: Uuid,
) -> Result<Vec<CollectionItemView>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            ci.id, ci.collection_id, ci.arrangement_id, ci.index,
            ci.created_at as "created_at: DateTime<Utc>",
            ci.updated_at as "updated_at: DateTime<Utc>",
            ci.created_by,
            ci.deleted_at as "deleted_at: DateTime<Utc>",
            a.slug as arrangement_slug,
            a.title as arrangement_title,
            (a.deleted_at IS NOT NULL) as "arrangement_removed!"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id
        JOIN arrangement a ON a.id = ci.arrangement_id
        WHERE ci.collection_id = $1 AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
        ORDER BY ci.index ASC, ci.id ASC
        "#,
        collection_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| CollectionItemView {
            item: CollectionItem {
                id: r.id,
                collection_id: r.collection_id,
                arrangement_id: r.arrangement_id,
                index: r.index,
                created_at: r.created_at,
                updated_at: r.updated_at,
                created_by: r.created_by,
                deleted_at: r.deleted_at,
            },
            arrangement_slug: r.arrangement_slug,
            arrangement_title: r.arrangement_title,
            arrangement_removed: r.arrangement_removed,
        })
        .collect())
}

/// A collection's *restorable* removed items, most-recently-removed first, plus
/// the total. Backs the console's "removed pieces" list.
///
/// Two filters make the list mean "these can come back":
///   - an arrangement that is live in the collection again is excluded — its
///     old row can never be restored ([`restore`] refuses it), so offering a
///     Restore button that always 409s would be a lie;
///   - where the same arrangement was removed more than once, only the most
///     recent removal is listed; restoring an older one would be restoring the
///     same piece by a different row.
///
/// Paginated so the query stays bounded however often a program has been
/// rebuilt.
pub async fn list_deleted_for_collection(
    pool: &PgPool,
    collection_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CollectionItemView>, i64), sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            ci.id, ci.collection_id, ci.arrangement_id, ci.index,
            ci.created_at as "created_at: DateTime<Utc>",
            ci.updated_at as "updated_at: DateTime<Utc>",
            ci.created_by,
            ci.deleted_at as "deleted_at: DateTime<Utc>",
            a.slug as arrangement_slug,
            a.title as arrangement_title,
            (a.deleted_at IS NOT NULL) as "arrangement_removed!"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id
        JOIN arrangement a ON a.id = ci.arrangement_id
        WHERE ci.collection_id = $1 AND ci.deleted_at IS NOT NULL AND c.deleted_at IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM collection_item live
              WHERE live.collection_id = ci.collection_id
                AND live.arrangement_id = ci.arrangement_id
                AND live.deleted_at IS NULL
          )
          AND ci.deleted_at = (
              SELECT max(newer.deleted_at) FROM collection_item newer
              WHERE newer.collection_id = ci.collection_id
                AND newer.arrangement_id = ci.arrangement_id
                AND newer.deleted_at IS NOT NULL
          )
        ORDER BY ci.deleted_at DESC, ci.id ASC
        LIMIT $2 OFFSET $3
        "#,
        collection_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) as "count!"
        FROM collection_item ci
        JOIN collection c ON c.id = ci.collection_id
        WHERE ci.collection_id = $1 AND ci.deleted_at IS NOT NULL AND c.deleted_at IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM collection_item live
              WHERE live.collection_id = ci.collection_id
                AND live.arrangement_id = ci.arrangement_id
                AND live.deleted_at IS NULL
          )
          AND ci.deleted_at = (
              SELECT max(newer.deleted_at) FROM collection_item newer
              WHERE newer.collection_id = ci.collection_id
                AND newer.arrangement_id = ci.arrangement_id
                AND newer.deleted_at IS NOT NULL
          )
        "#,
        collection_id,
    )
    .fetch_one(pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| CollectionItemView {
            item: CollectionItem {
                id: r.id,
                collection_id: r.collection_id,
                arrangement_id: r.arrangement_id,
                index: r.index,
                created_at: r.created_at,
                updated_at: r.updated_at,
                created_by: r.created_by,
                deleted_at: r.deleted_at,
            },
            arrangement_slug: r.arrangement_slug,
            arrangement_title: r.arrangement_title,
            arrangement_removed: r.arrangement_removed,
        })
        .collect();

    Ok((items, total))
}

/// Move a single item to `new_index`. Maps a clash on the existing index to
/// [`CollectionItemError::DuplicateIndex`]. Returns `None` if no live item
/// matched.
pub async fn update_index(
    pool: &PgPool,
    id: Uuid,
    new_index: i32,
) -> Result<Option<CollectionItem>, CollectionItemError> {
    let row = sqlx::query_as!(
        CollectionItem,
        r#"
        UPDATE collection_item
        SET index = $2, updated_at = now()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING
            id, collection_id, arrangement_id, index,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        new_index,
    )
    .fetch_optional(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row)
}

/// Reassign a whole collection's indices to `1..=n` in the order given by
/// `ordered_ids`, atomically. `ordered_ids` must be *exactly* a permutation of
/// the collection's current live item ids — no missing, extra, foreign, or
/// duplicate ids — else [`CollectionItemError::InvalidReorder`] and the
/// transaction rolls back (a partial list would otherwise silently corrupt the
/// order). Indices are parked out of range first so the per-row updates never
/// transiently collide on the unique `(collection_id, index)` index.
pub async fn reorder(
    pool: &PgPool,
    collection_id: Uuid,
    ordered_ids: &[Uuid],
) -> Result<(), CollectionItemError> {
    use std::collections::HashSet;

    let mut tx = pool.begin().await?;

    // Lock the collection's live items and confirm `ordered_ids` is a
    // permutation of them before touching any index.
    let live: Vec<Uuid> = sqlx::query_scalar!(
        r#"SELECT id FROM collection_item
           WHERE collection_id = $1 AND deleted_at IS NULL
           FOR UPDATE"#,
        collection_id,
    )
    .fetch_all(&mut *tx)
    .await?;

    let live_set: HashSet<Uuid> = live.iter().copied().collect();
    let ordered_set: HashSet<Uuid> = ordered_ids.iter().copied().collect();
    if ordered_ids.len() != live.len() || ordered_set != live_set {
        // Duplicates (len mismatch after dedupe), missing, or foreign ids.
        tx.rollback().await?;
        return Err(CollectionItemError::InvalidReorder);
    }

    // Park every live item's index far out of range to clear the space.
    sqlx::query!(
        r#"UPDATE collection_item SET index = index + $2
           WHERE collection_id = $1 AND deleted_at IS NULL"#,
        collection_id,
        INDEX_PARK_OFFSET,
    )
    .execute(&mut *tx)
    .await?;

    for (pos, id) in ordered_ids.iter().enumerate() {
        let new_index = (pos as i32) + 1;
        sqlx::query!(
            r#"UPDATE collection_item SET index = $1, updated_at = now()
               WHERE id = $2 AND collection_id = $3 AND deleted_at IS NULL"#,
            new_index,
            id,
            collection_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_write_error)?;
    }

    tx.commit().await?;
    Ok(())
}

/// Assign 1..n to `ordered_ids` inside an open transaction, parking the
/// collection's live indices out of range first so the per-row updates never
/// transiently collide on the unique `(collection_id, index)` index.
///
/// This is the one place piece numbers are written. Every operation that
/// changes the running order — reorder, remove, restore — ends here, which is
/// what keeps the numbers dense and increasing down the collection no matter
/// which one ran.
async fn renumber(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    collection_id: Uuid,
    ordered_ids: &[Uuid],
) -> Result<(), CollectionItemError> {
    park(tx, collection_id).await?;
    assign_order(tx, collection_id, ordered_ids).await
}

/// Move every live index of a collection out of the legal range, clearing the
/// unique space so the subsequent per-row writes cannot transiently collide.
/// Parked values land in `[1 + OFFSET, MAX_INDEX + OFFSET]`, which is why
/// [`MAX_INDEX`] must stay below the offset.
async fn park(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    collection_id: Uuid,
) -> Result<(), CollectionItemError> {
    sqlx::query!(
        r#"UPDATE collection_item SET index = index + $2
           WHERE collection_id = $1 AND deleted_at IS NULL"#,
        collection_id,
        INDEX_PARK_OFFSET,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Assign 1..n to already-parked rows.
async fn assign_order(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    collection_id: Uuid,
    ordered_ids: &[Uuid],
) -> Result<(), CollectionItemError> {
    for (position, id) in ordered_ids.iter().enumerate() {
        let new_index = (position as i32) + 1;
        sqlx::query!(
            r#"UPDATE collection_item SET index = $1, updated_at = now()
               WHERE id = $2 AND collection_id = $3 AND deleted_at IS NULL"#,
            new_index,
            id,
            collection_id,
        )
        .execute(&mut **tx)
        .await
        .map_err(map_write_error)?;
    }
    Ok(())
}

/// The collection's live item ids in index order, inside a transaction.
async fn live_ids_in_order(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    collection_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT id FROM collection_item
           WHERE collection_id = $1 AND deleted_at IS NULL
           ORDER BY index ASC, id ASC
           FOR UPDATE"#,
        collection_id,
    )
    .fetch_all(&mut **tx)
    .await
}

/// Remove a piece and close the gap it leaves: the pieces after it shift down
/// one, so the numbering stays dense and increasing.
///
/// Leaving the gap and letting the next reorder collapse it would mean the
/// numbers shown after a removal are not the numbers the collection actually
/// has — a standing collection is drawn from by number, so "there is no 3 any
/// more, count on" is not a state worth rendering.
///
/// Returns `false` if no live item matched.
pub async fn remove(
    pool: &PgPool,
    collection_id: Uuid,
    item_id: Uuid,
) -> Result<bool, CollectionItemError> {
    let mut tx = pool.begin().await?;

    let removed = sqlx::query!(
        r#"UPDATE collection_item SET deleted_at = now(), updated_at = now()
           WHERE id = $1 AND collection_id = $2 AND deleted_at IS NULL
           RETURNING id"#,
        item_id,
        collection_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if removed.is_none() {
        tx.rollback().await?;
        return Ok(false);
    }

    let remaining = live_ids_in_order(&mut tx, collection_id).await?;
    renumber(&mut tx, collection_id, &remaining).await?;
    tx.commit().await?;
    Ok(true)
}

/// Restore a removed piece at the position it held, shifting the pieces from
/// there on up one.
///
/// The number it had is where it belongs — restoring is undoing a removal, not
/// appending. If the collection has since shrunk below that number the piece
/// lands at the end instead, so the numbering stays dense.
///
/// Refuses with [`CollectionItemError::DuplicateArrangement`] when the same
/// arrangement has been re-added in the meantime: the collection would
/// otherwise list one piece twice.
///
/// Returns `false` if no removed item matched.
pub async fn restore(
    pool: &PgPool,
    collection_id: Uuid,
    item_id: Uuid,
) -> Result<bool, CollectionItemError> {
    let mut tx = pool.begin().await?;

    let Some(target) = sqlx::query!(
        r#"SELECT index, arrangement_id FROM collection_item
           WHERE id = $1 AND collection_id = $2 AND deleted_at IS NOT NULL
           FOR UPDATE"#,
        item_id,
        collection_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(false);
    };

    // The same arrangement may have been added again while this one was gone.
    let clash = sqlx::query_scalar!(
        r#"SELECT EXISTS(
               SELECT 1 FROM collection_item
               WHERE collection_id = $1 AND arrangement_id = $2 AND deleted_at IS NULL
           ) AS "clash!""#,
        collection_id,
        target.arrangement_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if clash {
        tx.rollback().await?;
        return Err(CollectionItemError::DuplicateArrangement);
    }

    let live = live_ids_in_order(&mut tx, collection_id).await?;

    // Park the live rows BEFORE reviving this one: its old number is very
    // likely taken by whichever piece shifted into it, and coming back at that
    // number would violate the unique index on the spot. Parking clears the
    // whole legal range first, and the row rejoins above the parked block —
    // `2 * INDEX_PARK_OFFSET` is out of reach of any parked value because
    // `MAX_INDEX < INDEX_PARK_OFFSET`.
    park(&mut tx, collection_id).await?;
    sqlx::query!(
        r#"UPDATE collection_item SET deleted_at = NULL, index = $2, updated_at = now()
           WHERE id = $1"#,
        item_id,
        2 * INDEX_PARK_OFFSET,
    )
    .execute(&mut *tx)
    .await
    .map_err(map_write_error)?;

    // Its old number, clamped to the end of a collection that has since shrunk.
    let position = (target.index.max(1) as usize - 1).min(live.len());
    let mut order = live;
    order.insert(position, item_id);
    assign_order(&mut tx, collection_id, &order).await?;

    tx.commit().await?;
    Ok(true)
}

/// Soft-delete an item without touching the other pieces' numbers. Prefer
/// [`remove`], which closes the gap; this stays for callers that manage the
/// numbering themselves.
///
/// Returns `false` if no live row matched.
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE collection_item SET deleted_at = now(), updated_at = now()
           WHERE id = $1 AND deleted_at IS NULL"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Undelete an item. Can fail with [`CollectionItemError::DuplicateIndex`] if
/// its old index was reused while it was gone. Returns `false` if no
/// soft-deleted row matched.
pub async fn undelete(pool: &PgPool, id: Uuid) -> Result<bool, CollectionItemError> {
    let result = sqlx::query!(
        r#"UPDATE collection_item SET deleted_at = NULL, updated_at = now()
           WHERE id = $1 AND deleted_at IS NOT NULL"#,
        id,
    )
    .execute(pool)
    .await
    .map_err(map_write_error)?;
    Ok(result.rows_affected() > 0)
}
