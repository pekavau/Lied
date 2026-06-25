//! Work: the abstract musical work (CLAUDE.md Entities: Work). Instance-wide
//! — no `organization_id`, no `deleted_at` (CLAUDE.md: "Works are durable
//! references, orphans are not garbage-collected"). No unique constraint:
//! duplicates allowed in phase 1.
//!
//! Permissions (CLAUDE.md Decisions): *creating* a Work is open to any
//! authenticated user with >= 1 `Membership` in any org; *editing* is
//! restricted to `created_by` or `is_system_admin` — enforced by
//! [`crate::routes::arrangements`], not here (this module is pure data
//! access), except for [`is_editable_by`], a small helper colocated with the
//! type it checks.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of a `work` row. `camelCase` per CLAUDE.md's JSON-
/// casing convention.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Work {
    pub id: Uuid,
    pub title: String,
    pub composer: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
}

impl Work {
    /// Whether `user_id` (and whether they are a system admin) may edit/
    /// delete this Work (CLAUDE.md: "editing a Work is restricted to its
    /// original creator ... or any `is_system_admin`").
    pub fn is_editable_by(&self, user_id: Uuid, is_system_admin: bool) -> bool {
        is_system_admin || self.created_by == Some(user_id)
    }
}

/// Insert a new Work row. Creation has no slug/uniqueness constraint
/// (CLAUDE.md: "no unique constraint -- duplicates allowed in phase 1").
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    title: &str,
    composer: Option<&str>,
    created_by: Option<Uuid>,
) -> Result<Work, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        INSERT INTO work (id, title, composer, created_by)
        VALUES ($1, $2, $3, $4)
        RETURNING
            id, title, composer, created_by,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        "#,
        id,
        title,
        composer,
        created_by,
    )
    .fetch_one(pool)
    .await?;

    Ok(Work {
        id: row.id,
        title: row.title,
        composer: row.composer,
        created_at: row.created_at,
        updated_at: row.updated_at,
        created_by: row.created_by,
    })
}

/// Look up a Work by id.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Work>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT
            id, title, composer, created_by,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        FROM work
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| Work {
        id: row.id,
        title: row.title,
        composer: row.composer,
        created_at: row.created_at,
        updated_at: row.updated_at,
        created_by: row.created_by,
    }))
}

/// Sort allowlist for `GET /v1/works`. Default sort is `title:asc`.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[
    ("title", "title"),
    ("composer", "composer"),
    ("createdAt", "created_at"),
];

/// Filter allowlist for `GET /v1/works`: `?filter[title]=`/`?filter[composer]=`
/// are case-insensitive substring matches (phase-1 ILIKE search).
pub const FILTER_ALLOWLIST: &[&str] = &["title", "composer"];

/// Fetch one page of Works plus the total row count, with an allowlisted
/// sort and optional `title`/`composer` ILIKE filters (ANDed together; see
/// [`crate::domain::organization::list`] for why the `format!`-built query
/// around fixed, allowlisted fragments is safe).
pub async fn list(
    pool: &PgPool,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    title_filter: Option<&str>,
    composer_filter: Option<&str>,
) -> Result<(Vec<Work>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    let query = format!(
        r#"
        SELECT
            id, title, composer, created_by,
            created_at,
            updated_at,
            count(*) OVER() as total
        FROM work
        WHERE ($3::text IS NULL OR title ILIKE '%' || $3 || '%')
          AND ($4::text IS NULL OR composer ILIKE '%' || $4 || '%')
        ORDER BY {sort_column} {direction}, id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(title_filter)
        .bind(composer_filter)
        .fetch_all(pool)
        .await?;

    use sqlx::Row;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            let count_query = r#"
                SELECT count(*) FROM work
                WHERE ($1::text IS NULL OR title ILIKE '%' || $1 || '%')
                  AND ($2::text IS NULL OR composer ILIKE '%' || $2 || '%')
            "#;
            sqlx::query_scalar::<_, i64>(count_query)
                .bind(title_filter)
                .bind(composer_filter)
                .fetch_one(pool)
                .await?
        }
    };

    let items = rows
        .into_iter()
        .map(|row| {
            Ok(Work {
                id: row.try_get("id")?,
                title: row.try_get("title")?,
                composer: row.try_get("composer")?,
                created_by: row.try_get("created_by")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// Update a Work's mutable fields (`title`, `composer`). Caller is
/// responsible for the [`Work::is_editable_by`] check before calling this.
/// Returns `None` if no row matched `id`.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    title: &str,
    composer: Option<&str>,
) -> Result<Option<Work>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        UPDATE work
        SET title = $2, composer = $3, updated_at = now()
        WHERE id = $1
        RETURNING
            id, title, composer, created_by,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        "#,
        id,
        title,
        composer,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| Work {
        id: row.id,
        title: row.title,
        composer: row.composer,
        created_at: row.created_at,
        updated_at: row.updated_at,
        created_by: row.created_by,
    }))
}

/// Hard-delete a Work row. CLAUDE.md gives Work no `deleted_at`, and the
/// only FK referencing it (`arrangement.work_id`) is nullable with no
/// `ON DELETE` clause, so deleting a Work still referenced by a live
/// Arrangement fails the FK constraint — callers should map that to a `409`
/// rather than a raw `500` (see [`user::delete`](super::user::delete) for the
/// same pattern).
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(r#"DELETE FROM work WHERE id = $1"#, id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creator_can_edit_own_work() {
        let creator = Uuid::now_v7();
        let work = Work {
            id: Uuid::now_v7(),
            title: "Symphony".to_string(),
            composer: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            created_by: Some(creator),
        };
        assert!(work.is_editable_by(creator, false));
    }

    #[test]
    fn non_creator_non_admin_cannot_edit() {
        let creator = Uuid::now_v7();
        let other = Uuid::now_v7();
        let work = Work {
            id: Uuid::now_v7(),
            title: "Symphony".to_string(),
            composer: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            created_by: Some(creator),
        };
        assert!(!work.is_editable_by(other, false));
    }

    #[test]
    fn system_admin_can_edit_anyones_work() {
        let creator = Uuid::now_v7();
        let admin = Uuid::now_v7();
        let work = Work {
            id: Uuid::now_v7(),
            title: "Symphony".to_string(),
            composer: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            created_by: Some(creator),
        };
        assert!(work.is_editable_by(admin, true));
    }
}
