//! CollectionSection: a heading over a range of numbers in a standing
//! collection (docs/design.md Entities: CollectionSection, and *Collection
//! numbering*). Issue #103.
//!
//! A section runs from its `starts_at` up to the next section's start;
//! which section a piece is in follows from its number (`65a` is in the
//! section covering 65), so items never point at a section and adding,
//! moving or deleting a section changes no number. Sections exist in
//! standing collections only.
//!
//! Every write runs under the collection lock
//! ([`collection_item::lock_collection`]), the same one every item write
//! takes, so sections and pieces never change under each other.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::collection_item::{self, CollectionItemError, Kind};

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

/// Wire representation of a `collection_section` row.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionSection {
    pub id: Uuid,
    pub collection_id: Uuid,
    /// "Märsche", "Polkas", "Modern".
    pub name: String,
    /// The first number the section covers; it runs until the next
    /// section's start.
    pub starts_at: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
}

#[derive(thiserror::Error, Debug)]
pub enum SectionError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    /// The collection does not exist or is soft-deleted.
    #[error("the collection does not exist")]
    CollectionGone,
    /// Sections are for standing collections; a program's numbers are its
    /// running order.
    #[error("only a standing collection has sections")]
    NotStanding,
    /// A start outside `1..=max` (the collection's width).
    #[error("startsAt must be between 1 and {max}")]
    InvalidStart { max: i32 },
    /// Another section of the collection starts there.
    #[error("another section already starts at {start}")]
    StartTaken { start: i32 },
}

impl From<CollectionItemError> for SectionError {
    fn from(err: CollectionItemError) -> Self {
        match err {
            CollectionItemError::Database(e) => SectionError::Database(e),
            // `lock_collection`, the only item function used here, fails
            // with nothing else.
            _ => SectionError::CollectionGone,
        }
    }
}

/// Lock the collection for a section write; it must be a standing one.
async fn lock_standing(tx: &mut Tx<'_>, collection_id: Uuid) -> Result<i16, SectionError> {
    let locked = collection_item::lock_collection(tx, collection_id).await?;
    if locked.kind != Kind::Standing {
        return Err(SectionError::NotStanding);
    }
    Ok(locked.number_width)
}

/// Refuse a start the collection cannot hold, or one another section has.
async fn check_start(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
    width: i16,
    starts_at: i32,
    except: Option<Uuid>,
) -> Result<(), SectionError> {
    let max = i32::from(collection_item::max_number(width));
    if !(1..=max).contains(&starts_at) {
        return Err(SectionError::InvalidStart { max });
    }
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS(
               SELECT 1 FROM collection_section
               WHERE collection_id = $1 AND starts_at = $2
                 AND ($3::uuid IS NULL OR id <> $3)
           ) AS "taken!""#,
        collection_id,
        starts_at,
        except,
    )
    .fetch_one(&mut **tx)
    .await?;
    if taken {
        return Err(SectionError::StartTaken { start: starts_at });
    }
    Ok(())
}

/// A collection's sections, by start. Unpaginated: a folder has a handful.
pub async fn list_for_collection(
    pool: &PgPool,
    collection_id: Uuid,
) -> Result<Vec<CollectionSection>, sqlx::Error> {
    sqlx::query_as!(
        CollectionSection,
        r#"
        SELECT id, collection_id, name, starts_at,
               created_at as "created_at: DateTime<Utc>",
               updated_at as "updated_at: DateTime<Utc>",
               created_by
        FROM collection_section
        WHERE collection_id = $1
        ORDER BY starts_at ASC
        "#,
        collection_id,
    )
    .fetch_all(pool)
    .await
}

/// Look up a section by id.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<CollectionSection>, sqlx::Error> {
    sqlx::query_as!(
        CollectionSection,
        r#"
        SELECT id, collection_id, name, starts_at,
               created_at as "created_at: DateTime<Utc>",
               updated_at as "updated_at: DateTime<Utc>",
               created_by
        FROM collection_section
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Add a section starting at `starts_at`. The start may lie past the
/// highest number (a new section after the others: "Modern" from 101 while
/// the polkas end at 75), which is what makes that number an allowed label,
/// or inside the numbers in use, dividing an existing folder: the pieces
/// from there on then fall into it, and no number changes.
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    collection_id: Uuid,
    name: &str,
    starts_at: i32,
    created_by: Option<Uuid>,
) -> Result<CollectionSection, SectionError> {
    let mut tx = pool.begin().await?;
    let width = lock_standing(&mut tx, collection_id).await?;
    check_start(&mut tx, collection_id, width, starts_at, None).await?;
    let row = sqlx::query_as!(
        CollectionSection,
        r#"
        INSERT INTO collection_section (id, collection_id, name, starts_at, created_by)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, collection_id, name, starts_at,
                  created_at as "created_at: DateTime<Utc>",
                  updated_at as "updated_at: DateTime<Utc>",
                  created_by
        "#,
        id,
        collection_id,
        name,
        starts_at,
        created_by,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Lock a section of the collection at the caller's version, under the
/// collection lock. `None`: no such section, or a stale version.
async fn lock_section(
    tx: &mut Tx<'_>,
    collection_id: Uuid,
    id: Uuid,
    expected_version: Option<i64>,
) -> Result<Option<CollectionSection>, sqlx::Error> {
    sqlx::query_as!(
        CollectionSection,
        r#"
        SELECT id, collection_id, name, starts_at,
               created_at as "created_at: DateTime<Utc>",
               updated_at as "updated_at: DateTime<Utc>",
               created_by
        FROM collection_section
        WHERE id = $1 AND collection_id = $2
          AND ($3::bigint IS NULL OR floor(extract(epoch FROM updated_at) * 1000)::bigint = $3)
        FOR UPDATE
        "#,
        id,
        collection_id,
        expected_version,
    )
    .fetch_optional(&mut **tx)
    .await
}

/// The fields a section update may change; `None` keeps the current value.
#[derive(Debug, Clone, Copy, Default)]
pub struct SectionChanges<'a> {
    pub name: Option<&'a str>,
    pub starts_at: Option<i32>,
}

/// Rename a section or move its start. Moving the start regroups the pieces
/// around it; no number changes. An update that changes nothing writes
/// nothing (the version stays). Returns `None` if no section matched at
/// `expected_version`.
///
/// `expected_version`: see [guarded writes](crate::domain#guarded-writes).
pub async fn update(
    pool: &PgPool,
    collection_id: Uuid,
    id: Uuid,
    expected_version: Option<i64>,
    changes: SectionChanges<'_>,
) -> Result<Option<CollectionSection>, SectionError> {
    let mut tx = pool.begin().await?;
    let width = lock_standing(&mut tx, collection_id).await?;
    let Some(current) = lock_section(&mut tx, collection_id, id, expected_version).await? else {
        return Ok(None);
    };
    let name = changes.name.unwrap_or(&current.name);
    let starts_at = changes.starts_at.unwrap_or(current.starts_at);
    if name == current.name && starts_at == current.starts_at {
        return Ok(Some(current));
    }
    if starts_at != current.starts_at {
        check_start(&mut tx, collection_id, width, starts_at, Some(id)).await?;
    }
    let row = sqlx::query_as!(
        CollectionSection,
        r#"
        UPDATE collection_section
        SET name = $2, starts_at = $3, updated_at = now()
        WHERE id = $1
        RETURNING id, collection_id, name, starts_at,
                  created_at as "created_at: DateTime<Utc>",
                  updated_at as "updated_at: DateTime<Utc>",
                  created_by
        "#,
        id,
        name,
        starts_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(row))
}

/// Delete a section (a hard delete): only the heading goes; its pieces fall
/// into the section before it, and no number changes. Returns `false` if no
/// section matched at `expected_version`.
///
/// `expected_version`: see [guarded writes](crate::domain#guarded-writes).
pub async fn delete(
    pool: &PgPool,
    collection_id: Uuid,
    id: Uuid,
    expected_version: Option<i64>,
) -> Result<bool, SectionError> {
    let mut tx = pool.begin().await?;
    // A program never has sections, so take the lock without the type
    // check: a stray row (there cannot be one) could still be removed.
    collection_item::lock_collection(&mut tx, collection_id).await?;
    let result = sqlx::query!(
        r#"DELETE FROM collection_section
           WHERE id = $1 AND collection_id = $2
             AND ($3::bigint IS NULL OR floor(extract(epoch FROM updated_at) * 1000)::bigint = $3)"#,
        id,
        collection_id,
        expected_version,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}
