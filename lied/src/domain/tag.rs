//! Tag: an open-ended classifier scoped to an organization, plus
//! ArrangementTag, the many-to-many join to Arrangement (CLAUDE.md Entities:
//! Tag, ArrangementTag). Issue #6 adds full CRUD on Tag and attach/detach on
//! the join, gated to `owner`/`archivist` (CLAUDE.md Permission matrix:
//! "Manage tags").
//!
//! `Tag.kind` stays free-text by design (no CHECK) — CLAUDE.md: "orgs can
//! invent categories without a migration."

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of a `tag` row. `camelCase` per CLAUDE.md's JSON-
/// casing convention.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub kind: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(thiserror::Error, Debug)]
pub enum TagError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("a tag with this name and kind already exists in this organization")]
    Duplicate,
}

fn map_write_error(err: sqlx::Error) -> TagError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("tag_organization_id_name_kind_key") {
            return TagError::Duplicate;
        }
    }
    TagError::Database(err)
}

struct Row {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    kind: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    deleted_at: Option<DateTime<Utc>>,
}

impl Row {
    fn into_tag(self) -> Tag {
        Tag {
            id: self.id,
            organization_id: self.organization_id,
            name: self.name,
            kind: self.kind,
            created_at: self.created_at,
            updated_at: self.updated_at,
            created_by: self.created_by,
            deleted_at: self.deleted_at,
        }
    }
}

/// Insert a new tag row.
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    organization_id: Uuid,
    name: &str,
    kind: Option<&str>,
    created_by: Option<Uuid>,
) -> Result<Tag, TagError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        INSERT INTO tag (id, organization_id, name, kind, created_by)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING
            id, organization_id, name, kind,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        organization_id,
        name,
        kind,
        created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row.into_tag())
}

/// Look up a tag by id. Hides soft-deleted rows.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Tag>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT
            id, organization_id, name, kind,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM tag
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_tag))
}

/// Sort allowlist for `GET /v1/orgs/{orgId}/tags`. Default sort is
/// `name:asc`.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[("name", "name"), ("kind", "kind")];

/// Filter allowlist: exact match on `kind`.
pub const FILTER_ALLOWLIST: &[&str] = &["kind"];

/// Fetch one page of an organization's (live) tags.
pub async fn list_for_org(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    kind_filter: Option<&str>,
) -> Result<(Vec<Tag>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    let query = format!(
        r#"
        SELECT
            id, organization_id, name, kind,
            created_at, updated_at, created_by, deleted_at,
            count(*) OVER() as total
        FROM tag
        WHERE organization_id = $3
          AND deleted_at IS NULL
          AND ($4::text IS NULL OR kind = $4)
        ORDER BY {sort_column} {direction}, id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(organization_id)
        .bind(kind_filter)
        .fetch_all(pool)
        .await?;

    use sqlx::Row as SqlxRow;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            let count_query = r#"
                SELECT count(*) FROM tag
                WHERE organization_id = $1 AND deleted_at IS NULL
                  AND ($2::text IS NULL OR kind = $2)
            "#;
            sqlx::query_scalar::<_, i64>(count_query)
                .bind(organization_id)
                .bind(kind_filter)
                .fetch_one(pool)
                .await?
        }
    };

    let items = rows
        .into_iter()
        .map(|row| {
            Ok(Tag {
                id: row.try_get("id")?,
                organization_id: row.try_get("organization_id")?,
                name: row.try_get("name")?,
                kind: row.try_get("kind")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
                created_by: row.try_get("created_by")?,
                deleted_at: row.try_get("deleted_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// Update a tag's mutable fields (`name`, `kind`). Returns `None` if no live
/// row matched `id`.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    kind: Option<&str>,
) -> Result<Option<Tag>, TagError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        UPDATE tag
        SET name = $2, kind = $3, updated_at = now()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING
            id, organization_id, name, kind,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        name,
        kind,
    )
    .fetch_optional(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row.map(Row::into_tag))
}

/// Soft-delete a tag (`deleted_at` on this row only). The owning
/// `arrangement_tag` join rows are left untouched (no soft-delete on that
/// entity per CLAUDE.md) — a soft-deleted tag's joins become invisible
/// through [`list_tags_for_arrangement`]'s `tag.deleted_at IS NULL` filter
/// without the join row itself being modified, the same hide-with-references
/// pattern as Arrangement -> Voice.
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE tag SET deleted_at = now(), updated_at = now() WHERE id = $1 AND deleted_at IS NULL"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Undelete a tag.
pub async fn undelete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE tag SET deleted_at = NULL, updated_at = now() WHERE id = $1 AND deleted_at IS NOT NULL"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// ArrangementTag: many-to-many join. No soft-delete on the join itself
// (CLAUDE.md) — attach/detach are real inserts/deletes.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum ArrangementTagError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("this tag is already attached to this arrangement")]
    Duplicate,
}

/// Attach a tag to an arrangement. Idempotent failure mode: a duplicate
/// attach is a clean `409`, not a raw constraint-violation `500`.
pub async fn attach(
    pool: &PgPool,
    id: Uuid,
    arrangement_id: Uuid,
    tag_id: Uuid,
) -> Result<(), ArrangementTagError> {
    sqlx::query!(
        r#"
        INSERT INTO arrangement_tag (id, arrangement_id, tag_id)
        VALUES ($1, $2, $3)
        "#,
        id,
        arrangement_id,
        tag_id,
    )
    .execute(pool)
    .await
    .map_err(|err| {
        if let sqlx::Error::Database(ref db_err) = err {
            if db_err.constraint() == Some("arrangement_tag_arrangement_id_tag_id_key") {
                return ArrangementTagError::Duplicate;
            }
        }
        ArrangementTagError::Database(err)
    })?;
    Ok(())
}

/// Detach a tag from an arrangement. Returns `false` if no such join row
/// existed.
pub async fn detach(
    pool: &PgPool,
    arrangement_id: Uuid,
    tag_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"DELETE FROM arrangement_tag WHERE arrangement_id = $1 AND tag_id = $2"#,
        arrangement_id,
        tag_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// List the (live) tags attached to an arrangement — joins through
/// `arrangement_tag`, filtering out tags that are themselves soft-deleted
/// (hide-with-references: the join row survives, the tag becomes invisible
/// through it).
pub async fn list_tags_for_arrangement(
    pool: &PgPool,
    arrangement_id: Uuid,
) -> Result<Vec<Tag>, sqlx::Error> {
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT
            t.id, t.organization_id, t.name, t.kind,
            t.created_at as "created_at: DateTime<Utc>",
            t.updated_at as "updated_at: DateTime<Utc>",
            t.created_by,
            t.deleted_at as "deleted_at: DateTime<Utc>"
        FROM tag t
        JOIN arrangement_tag at ON at.tag_id = t.id
        WHERE at.arrangement_id = $1 AND t.deleted_at IS NULL
        ORDER BY t.name ASC, t.id ASC
        "#,
        arrangement_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(Row::into_tag).collect())
}
