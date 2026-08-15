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
}

fn map_write_error(err: sqlx::Error) -> CollectionItemError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("collection_item_collection_id_index_key") {
            return CollectionItemError::DuplicateIndex;
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

/// A collection's soft-deleted items, most-recently-removed first, plus the
/// total. Backs the console's "removed pieces" restore list. Paginated so the
/// query stays bounded however often a program has been rebuilt.
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
        r#"UPDATE collection_item SET index = index + 1000000
           WHERE collection_id = $1 AND deleted_at IS NULL"#,
        collection_id,
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

/// Soft-delete an item (its slot frees up for reuse). Returns `false` if no
/// live row matched.
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
