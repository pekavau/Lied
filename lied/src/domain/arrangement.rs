//! Arrangement: a specific arrangement for specific instrumentation, owned
//! by an organization (CLAUDE.md Entities: Arrangement). Issue #6 adds full
//! CRUD, gated to `owner`/`archivist` (CLAUDE.md Permission matrix:
//! "Upload/edit arrangements & files").
//!
//! **Soft-delete (hide-with-references):** `deleted_at` hides the row from
//! `find_by_id`/`list`/search. Undelete clears it. CLAUDE.md: a soft-delete
//! sets `deleted_at` on the targeted entity ONLY, never its descendants
//! (Voices) — see [`crate::domain::voice`] for the corresponding
//! `WHERE arrangement.deleted_at IS NULL` join filter that hides a deleted
//! arrangement's voices *through it* without touching their own state.

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of an `arrangement` row. `camelCase` per CLAUDE.md's
/// JSON-casing convention.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Arrangement {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub title: String,
    pub slug: String,
    pub work_id: Option<Uuid>,
    pub instrumentation: Option<String>,
    pub arranger: Option<String>,
    pub publisher: Option<String>,
    pub purchase_date: Option<NaiveDate>,
    pub license_notes: Option<String>,
    pub copy_count_allowed: Option<i32>,
    pub status: String,
    pub duration_seconds: Option<i32>,
    pub difficulty: Option<i16>,
    pub difficulty_ratings: Option<serde_json::Value>,
    pub difficulty_notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(thiserror::Error, Debug)]
pub enum ArrangementError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("slug already exists in this organization")]
    DuplicateSlug,
    #[error("work_id does not reference a live work")]
    UnknownWork,
}

fn map_write_error(err: sqlx::Error) -> ArrangementError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("arrangement_organization_id_slug_key") {
            return ArrangementError::DuplicateSlug;
        }
        if db_err.is_foreign_key_violation() {
            return ArrangementError::UnknownWork;
        }
    }
    ArrangementError::Database(err)
}

/// Slugify an arrangement title (CLAUDE.md: slugs are generated from the
/// title/name at creation and are immutable thereafter). Reuses the
/// shared ASCII/lowercase/hyphenate algorithm — see
/// [`crate::domain::user::slugify`].
pub fn slugify(input: &str) -> String {
    crate::domain::user::slugify(input)
}

/// Mutable fields shared by create/update — bundled so positional args don't
/// drift as the field count grows (CLAUDE.md: many provenance/difficulty
/// fields on this entity).
pub struct ArrangementFields<'a> {
    pub title: &'a str,
    pub work_id: Option<Uuid>,
    pub instrumentation: Option<&'a str>,
    pub arranger: Option<&'a str>,
    pub publisher: Option<&'a str>,
    pub purchase_date: Option<NaiveDate>,
    pub license_notes: Option<&'a str>,
    pub copy_count_allowed: Option<i32>,
    pub status: &'a str,
    pub duration_seconds: Option<i32>,
    pub difficulty: Option<i16>,
    pub difficulty_ratings: Option<serde_json::Value>,
    pub difficulty_notes: Option<&'a str>,
}

struct Row {
    id: Uuid,
    organization_id: Uuid,
    title: String,
    slug: String,
    work_id: Option<Uuid>,
    instrumentation: Option<String>,
    arranger: Option<String>,
    publisher: Option<String>,
    purchase_date: Option<NaiveDate>,
    license_notes: Option<String>,
    copy_count_allowed: Option<i32>,
    status: String,
    duration_seconds: Option<i32>,
    difficulty: Option<i16>,
    difficulty_ratings: Option<serde_json::Value>,
    difficulty_notes: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    deleted_at: Option<DateTime<Utc>>,
}

impl Row {
    fn into_arrangement(self) -> Arrangement {
        Arrangement {
            id: self.id,
            organization_id: self.organization_id,
            title: self.title,
            slug: self.slug,
            work_id: self.work_id,
            instrumentation: self.instrumentation,
            arranger: self.arranger,
            publisher: self.publisher,
            purchase_date: self.purchase_date,
            license_notes: self.license_notes,
            copy_count_allowed: self.copy_count_allowed,
            status: self.status,
            duration_seconds: self.duration_seconds,
            difficulty: self.difficulty,
            difficulty_ratings: self.difficulty_ratings,
            difficulty_notes: self.difficulty_notes,
            created_at: self.created_at,
            updated_at: self.updated_at,
            created_by: self.created_by,
            deleted_at: self.deleted_at,
        }
    }
}

/// Insert a new arrangement row.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    organization_id: Uuid,
    slug: &str,
    fields: ArrangementFields<'_>,
    created_by: Option<Uuid>,
) -> Result<Arrangement, ArrangementError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        INSERT INTO arrangement (
            id, organization_id, title, slug, work_id, instrumentation, arranger,
            publisher, purchase_date, license_notes, copy_count_allowed, status,
            duration_seconds, difficulty, difficulty_ratings, difficulty_notes,
            created_by
        )
        -- purchase_date is bound as text and cast text->date so sqlx infers a
        -- `String` param: with the `time` feature pulled in transitively (by
        -- tower-sessions-sqlx-store) the macro would otherwise demand a
        -- `time::Date` for a `date` param, clashing with our chrono NaiveDate.
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::text::date, $10, $11, $12, $13, $14, $15, $16, $17)
        RETURNING
            id, organization_id, title, slug, work_id, instrumentation, arranger,
            publisher,
            purchase_date as "purchase_date: NaiveDate",
            license_notes, copy_count_allowed, status,
            duration_seconds, difficulty, difficulty_ratings, difficulty_notes,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        organization_id,
        fields.title,
        slug,
        fields.work_id,
        fields.instrumentation,
        fields.arranger,
        fields.publisher,
        fields.purchase_date.map(|d| d.to_string()),
        fields.license_notes,
        fields.copy_count_allowed,
        fields.status,
        fields.duration_seconds,
        fields.difficulty,
        fields.difficulty_ratings,
        fields.difficulty_notes,
        created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row.into_arrangement())
}

/// Look up an arrangement by id. Hides soft-deleted rows (CLAUDE.md soft
/// delete: hidden from REST/WebDAV/search).
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Arrangement>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT
            id, organization_id, title, slug, work_id, instrumentation, arranger,
            publisher,
            purchase_date as "purchase_date: NaiveDate",
            license_notes, copy_count_allowed, status,
            duration_seconds, difficulty, difficulty_ratings, difficulty_notes,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM arrangement
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_arrangement))
}

/// Look up an arrangement by id, including soft-deleted rows — used by
/// undelete (which must find the very row `find_by_id` hides) and by admin
/// flows that need to confirm an id existed at all.
pub async fn find_by_id_including_deleted(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Arrangement>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT
            id, organization_id, title, slug, work_id, instrumentation, arranger,
            publisher,
            purchase_date as "purchase_date: NaiveDate",
            license_notes, copy_count_allowed, status,
            duration_seconds, difficulty, difficulty_ratings, difficulty_notes,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM arrangement
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_arrangement))
}

/// Sort allowlist for `GET /v1/orgs/{orgId}/arrangements`. Default sort is
/// `title:asc` (CLAUDE.md: "arrangements default `title:asc`").
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[
    ("title", "a.title"),
    ("createdAt", "a.created_at"),
    ("difficulty", "a.difficulty"),
    ("durationSeconds", "a.duration_seconds"),
];

/// Filter allowlist: `status` (exact match) plus the phase-1 `?q=` ILIKE
/// search is handled separately (not via the `filter[...]` bracket syntax —
/// CLAUDE.md: "Phase-1 ILIKE search stays a separate `?q=` param").
pub const FILTER_ALLOWLIST: &[&str] = &["status"];

/// Fetch one page of an organization's arrangements plus the total live row
/// count, with an allowlisted sort, optional exact `status` filter, and
/// optional `q` ILIKE search over `Arrangement.title` and `Work.composer`
/// (CLAUDE.md: "ILIKE search on `Arrangement.title` and `Work.composer`
/// (via the optional Work join)"). Soft-deleted arrangements are always
/// excluded; their join through the optional Work row is also excluded if
/// that Work itself were ever made soft-deletable (it isn't, per the data
/// model, so no `deleted_at` filter applies to `work`).
#[allow(clippy::too_many_arguments)]
pub async fn list_for_org(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    status_filter: Option<&str>,
    q: Option<&str>,
) -> Result<(Vec<Arrangement>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    // Runtime `sqlx::query` (dynamic allowlisted ORDER BY) — see
    // `organization::list` for why building this with `format!` around
    // fixed, allowlisted fragments is safe.
    let query = format!(
        r#"
        SELECT
            a.id, a.organization_id, a.title, a.slug, a.work_id, a.instrumentation,
            a.arranger, a.publisher, a.purchase_date, a.license_notes,
            a.copy_count_allowed, a.status, a.duration_seconds, a.difficulty,
            a.difficulty_ratings, a.difficulty_notes,
            a.created_at, a.updated_at, a.created_by, a.deleted_at,
            count(*) OVER() as total
        FROM arrangement a
        LEFT JOIN work w ON w.id = a.work_id
        WHERE a.organization_id = $3
          AND a.deleted_at IS NULL
          AND ($4::text IS NULL OR a.status = $4)
          AND ($5::text IS NULL OR a.title ILIKE '%' || $5 || '%' OR w.composer ILIKE '%' || $5 || '%')
        ORDER BY {sort_column} {direction}, a.id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(organization_id)
        .bind(status_filter)
        .bind(q)
        .fetch_all(pool)
        .await?;

    use sqlx::Row as SqlxRow;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            let count_query = r#"
                SELECT count(*) FROM arrangement a
                LEFT JOIN work w ON w.id = a.work_id
                WHERE a.organization_id = $1
                  AND a.deleted_at IS NULL
                  AND ($2::text IS NULL OR a.status = $2)
                  AND ($3::text IS NULL OR a.title ILIKE '%' || $3 || '%' OR w.composer ILIKE '%' || $3 || '%')
            "#;
            sqlx::query_scalar::<_, i64>(count_query)
                .bind(organization_id)
                .bind(status_filter)
                .bind(q)
                .fetch_one(pool)
                .await?
        }
    };

    let items = rows
        .into_iter()
        .map(|row| {
            Ok(Arrangement {
                id: row.try_get("id")?,
                organization_id: row.try_get("organization_id")?,
                title: row.try_get("title")?,
                slug: row.try_get("slug")?,
                work_id: row.try_get("work_id")?,
                instrumentation: row.try_get("instrumentation")?,
                arranger: row.try_get("arranger")?,
                publisher: row.try_get("publisher")?,
                purchase_date: row.try_get("purchase_date")?,
                license_notes: row.try_get("license_notes")?,
                copy_count_allowed: row.try_get("copy_count_allowed")?,
                status: row.try_get("status")?,
                duration_seconds: row.try_get("duration_seconds")?,
                difficulty: row.try_get("difficulty")?,
                difficulty_ratings: row.try_get("difficulty_ratings")?,
                difficulty_notes: row.try_get("difficulty_notes")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
                created_by: row.try_get("created_by")?,
                deleted_at: row.try_get("deleted_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// Update an arrangement's mutable fields. `slug` is immutable and not
/// accepted here (CLAUDE.md: slug rename is a separate, explicit, audited
/// op — not implemented in this item). Returns `None` if no live row
/// matched `id`.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    fields: ArrangementFields<'_>,
) -> Result<Option<Arrangement>, ArrangementError> {
    let row = sqlx::query_as!(
        Row,
        r#"
        UPDATE arrangement
        SET title = $2, work_id = $3, instrumentation = $4, arranger = $5,
            publisher = $6, purchase_date = $7::text::date, license_notes = $8,
            copy_count_allowed = $9, status = $10, duration_seconds = $11,
            difficulty = $12, difficulty_ratings = $13, difficulty_notes = $14,
            updated_at = now()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING
            id, organization_id, title, slug, work_id, instrumentation, arranger,
            publisher,
            purchase_date as "purchase_date: NaiveDate",
            license_notes, copy_count_allowed, status,
            duration_seconds, difficulty, difficulty_ratings, difficulty_notes,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        fields.title,
        fields.work_id,
        fields.instrumentation,
        fields.arranger,
        fields.publisher,
        fields.purchase_date.map(|d| d.to_string()),
        fields.license_notes,
        fields.copy_count_allowed,
        fields.status,
        fields.duration_seconds,
        fields.difficulty,
        fields.difficulty_ratings,
        fields.difficulty_notes,
    )
    .fetch_optional(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row.map(Row::into_arrangement))
}

/// Soft-delete: sets `deleted_at` on this row ONLY (CLAUDE.md
/// hide-with-references: never cascades to Voices). Returns `false` if no
/// live row matched `id`.
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE arrangement SET deleted_at = now(), updated_at = now() WHERE id = $1 AND deleted_at IS NULL"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Undelete: clears `deleted_at`. Returns `false` if no soft-deleted row
/// matched `id` (including if `id` doesn't exist at all, or is already
/// live).
pub async fn undelete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE arrangement SET deleted_at = NULL, updated_at = now() WHERE id = $1 AND deleted_at IS NOT NULL"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_matches_shared_algorithm() {
        assert_eq!(slugify("Symphony No. 5"), "symphony-no-5");
    }
}
