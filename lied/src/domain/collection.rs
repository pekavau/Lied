//! Collection: a named, indexed set of arrangements belonging to an
//! organization (docs/design.md Entities: Collection). Issue #9 adds CRUD +
//! soft-delete/undelete, gated to `owner`/`archivist`/`conductor` (docs/design.md
//! Permission matrix: "Build/edit collections").
//!
//! A collection is a `program` (fixed concert sequence) or `standing`
//! (drawn-from-flexibly repertoire); both are indexed by a local piece number
//! on their [`crate::domain::collection_item`]s.
//!
//! **Soft-delete (hide-with-references):** `deleted_at` hides the row; its
//! `CollectionItem`s are hidden *through it* by the `WHERE
//! collection.deleted_at IS NULL` join filter, without touching their own
//! state (same pattern as Arrangement → Voice).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// The two collection kinds (docs/design.md: `program` | `standing`). Stored as
/// `text` + `CHECK` per the enum convention; validated before write.
pub const COLLECTION_TYPES: &[&str] = &["program", "standing"];

/// Whether `value` is a valid collection type.
pub fn is_valid_type(value: &str) -> bool {
    COLLECTION_TYPES.contains(&value)
}

/// Wire representation of a `collection` row. `camelCase` per docs/design.md's
/// JSON-casing convention.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub slug: String,
    /// `program` | `standing`.
    #[serde(rename = "type")]
    pub collection_type: String,
    /// Digits in the WebDAV folder names of its pieces (`007-…` or
    /// `0007-…`), and so the highest label: 999 or 9999.
    pub number_width: i16,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(thiserror::Error, Debug)]
pub enum CollectionError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("slug already exists in this organization")]
    DuplicateSlug,
    /// A narrower `number_width` than a live piece's label needs.
    #[error("a piece is numbered {highest}, which does not fit {width} digits")]
    LabelsTooWide { highest: u16, width: i16 },
}

/// The allowed `number_width` values.
pub fn is_valid_number_width(width: i16) -> bool {
    width == 3 || width == 4
}

fn map_write_error(err: sqlx::Error) -> CollectionError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("collection_organization_id_slug_key") {
            return CollectionError::DuplicateSlug;
        }
    }
    CollectionError::Database(err)
}

/// Slugify a collection name (immutable once created, per docs/design.md).
pub fn slugify(input: &str) -> String {
    crate::slug::slugify(input, "collection")
}

/// Sort allowlist for `GET /v1/orgs/{orgId}/collections`. Default `name:asc`.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[("name", "name"), ("createdAt", "created_at")];

/// The sort a list uses when the request names none: (SQL column from
/// [`SORT_ALLOWLIST`], direction). The OpenAPI description must name it;
/// `sort_and_filter_docs_match_the_allowlists` checks that it does.
pub const DEFAULT_SORT: (&str, crate::listing::SortDirection) =
    ("name", crate::listing::SortDirection::Asc);

/// Filter allowlist: exact `type` match.
pub const FILTER_ALLOWLIST: &[&str] = &["type"];

/// Insert a new collection row.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    organization_id: Uuid,
    name: &str,
    slug: &str,
    collection_type: &str,
    number_width: i16,
    created_by: Option<Uuid>,
) -> Result<Collection, CollectionError> {
    let row = sqlx::query_as!(
        Collection,
        r#"
        INSERT INTO collection (id, organization_id, name, slug, type, number_width, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING
            id, organization_id, name, slug,
            type as "collection_type!",
            number_width,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        organization_id,
        name,
        slug,
        collection_type,
        number_width,
        created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row)
}

/// Look up a live collection by id (soft-deleted hidden).
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Collection>, sqlx::Error> {
    sqlx::query_as!(
        Collection,
        r#"
        SELECT
            id, organization_id, name, slug,
            type as "collection_type!",
            number_width,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Look up a collection by id including soft-deleted rows (undelete flow).
pub async fn find_by_id_including_deleted(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Collection>, sqlx::Error> {
    sqlx::query_as!(
        Collection,
        r#"
        SELECT
            id, organization_id, name, slug,
            type as "collection_type!",
            number_width,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// One page of an organization's collections + total live count, with an
/// allowlisted sort and optional exact `type` filter. Soft-deleted excluded.
pub async fn list_for_org(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    type_filter: Option<&str>,
) -> Result<(Vec<Collection>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    // Dynamic ORDER BY over allowlisted, fixed fragments only (see
    // `arrangement::list_for_org` for why this is injection-safe).
    let query = format!(
        r#"
        SELECT
            id, organization_id, name, slug, type, number_width,
            created_at, updated_at, created_by, deleted_at,
            count(*) OVER() as total
        FROM collection
        WHERE organization_id = $3
          AND deleted_at IS NULL
          AND ($4::text IS NULL OR type = $4)
        ORDER BY {sort_column} {direction}, id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(organization_id)
        .bind(type_filter)
        .fetch_all(pool)
        .await?;

    use sqlx::Row as SqlxRow;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            sqlx::query_scalar::<_, i64>(
                r#"SELECT count(*) FROM collection
                   WHERE organization_id = $1 AND deleted_at IS NULL
                     AND ($2::text IS NULL OR type = $2)"#,
            )
            .bind(organization_id)
            .bind(type_filter)
            .fetch_one(pool)
            .await?
        }
    };

    let items = rows
        .into_iter()
        .map(|row| {
            Ok(Collection {
                id: row.try_get("id")?,
                organization_id: row.try_get("organization_id")?,
                name: row.try_get("name")?,
                slug: row.try_get("slug")?,
                collection_type: row.try_get("type")?,
                number_width: row.try_get("number_width")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
                created_by: row.try_get("created_by")?,
                deleted_at: row.try_get("deleted_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// The org's soft-deleted collections, most-recently-deleted first, plus the
/// total count. Powers the console's "recently deleted" restore list. Paginated
/// so the query stays bounded however long an org has been curating.
pub async fn list_deleted_for_org(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Collection>, i64), sqlx::Error> {
    let rows = sqlx::query_as!(
        Collection,
        r#"
        SELECT
            id, organization_id, name, slug,
            type as "collection_type!",
            number_width,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM collection
        WHERE organization_id = $1 AND deleted_at IS NOT NULL
        ORDER BY deleted_at DESC, id ASC
        LIMIT $2 OFFSET $3
        "#,
        organization_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"SELECT count(*) as "count!" FROM collection
           WHERE organization_id = $1 AND deleted_at IS NOT NULL"#,
        organization_id,
    )
    .fetch_one(pool)
    .await?;

    Ok((rows, total))
}

/// The mutable fields of a collection, for [`update`].
#[derive(Debug, Clone, Copy)]
pub struct CollectionFields<'a> {
    pub name: &'a str,
    pub collection_type: &'a str,
    pub number_width: i16,
}

/// Update a collection's mutable fields (`name`, `type`, `number_width`).
/// `slug` is immutable. Returns `None` if no live row matched.
///
/// Runs under the same per-collection lock as every item write
/// ([`collection_item`](crate::domain::collection_item)), because two of the
/// fields change what the items' numbers mean:
///
/// - **type** program → standing keeps the numbers as labels; standing →
///   program closes the gaps (a renumber), since a program's labels are its
///   positions;
/// - **number_width** may always widen; narrowing is refused
///   ([`CollectionError::LabelsTooWide`]) while a live label would not fit.
///
/// `expected_version`: see [guarded writes](crate::domain#guarded-writes).
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    expected_version: Option<i64>,
    fields: CollectionFields<'_>,
) -> Result<Option<Collection>, CollectionError> {
    use crate::domain::collection_item;

    let mut tx = pool.begin().await?;
    let Some(current) = sqlx::query!(
        r#"SELECT type as "collection_type!", number_width FROM collection
           WHERE id = $1 AND deleted_at IS NULL
             AND ($2::bigint IS NULL OR floor(extract(epoch FROM updated_at) * 1000)::bigint = $2)
           FOR NO KEY UPDATE"#,
        id,
        expected_version,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };

    if fields.number_width < current.number_width {
        let highest = collection_item::highest_number_in(&mut tx, id).await?;
        if highest > collection_item::max_number(fields.number_width) {
            return Err(CollectionError::LabelsTooWide {
                highest,
                width: fields.number_width,
            });
        }
    }
    if current.collection_type == "standing" && fields.collection_type == "program" {
        collection_item::close_gaps_for_program(&mut tx, id).await?;
    }

    let row = sqlx::query_as!(
        Collection,
        r#"
        UPDATE collection
        SET name = $2, type = $3, number_width = $4, updated_at = now()
        WHERE id = $1
        RETURNING
            id, organization_id, name, slug,
            type as "collection_type!",
            number_width,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        fields.name,
        fields.collection_type,
        fields.number_width,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(map_write_error)?;
    tx.commit().await.map_err(map_write_error)?;

    Ok(Some(row))
}

/// Soft-delete: sets `deleted_at` on this collection only (its items are
/// hidden through it). Returns `false` if no live row matched.
///
/// `expected_version`: see [guarded writes](crate::domain#guarded-writes).
pub async fn soft_delete(
    pool: &PgPool,
    id: Uuid,
    expected_version: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE collection SET deleted_at = now(), updated_at = now()
           WHERE id = $1 AND deleted_at IS NULL AND ($2::bigint IS NULL OR floor(extract(epoch FROM updated_at) * 1000)::bigint = $2)"#,
        id,
        expected_version,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Undelete: clears `deleted_at`. Returns `false` if no soft-deleted row
/// matched. Can fail with [`CollectionError::DuplicateSlug`] if the slug was
/// reused by a new live collection while this one was soft-deleted (the partial
/// unique index only covers live rows).
pub async fn undelete(pool: &PgPool, id: Uuid) -> Result<bool, CollectionError> {
    let result = sqlx::query!(
        r#"UPDATE collection SET deleted_at = NULL, updated_at = now()
           WHERE id = $1 AND deleted_at IS NOT NULL"#,
        id,
    )
    .execute(pool)
    .await
    .map_err(map_write_error)?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_validation() {
        assert!(is_valid_type("program"));
        assert!(is_valid_type("standing"));
        assert!(!is_valid_type("mixtape"));
    }
}
