//! Voice: an individual instrument part within an arrangement (CLAUDE.md
//! Entities: Voice). Issue #6 adds full CRUD, gated to `owner`/`archivist`
//! (same Permission-matrix row as Arrangement: "Upload/edit arrangements &
//! files").
//!
//! **Soft-delete (hide-with-references):** mirrors
//! [`crate::domain::arrangement`] — `deleted_at` hides the row only; a
//! soft-deleted *parent* Arrangement additionally hides its Voices, but only
//! through the `arrangement.deleted_at IS NULL` join filter in
//! [`list_for_arrangement`]/[`find_by_id`] — the Voice's own `deleted_at`
//! stays untouched either way, so undeleting the Arrangement makes the Voice
//! reappear automatically without ambiguity about whether the Voice was
//! independently deleted.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of a `voice` row. `camelCase` per CLAUDE.md's JSON-
/// casing convention.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Voice {
    pub id: Uuid,
    pub arrangement_id: Uuid,
    pub name: String,
    pub slug: String,
    pub instrument_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(thiserror::Error, Debug)]
pub enum VoiceError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("slug already exists in this arrangement")]
    DuplicateSlug,
    #[error("instrument_id does not reference a live instrument")]
    UnknownInstrument,
}

fn map_write_error(err: sqlx::Error) -> VoiceError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("voice_arrangement_id_slug_key") {
            return VoiceError::DuplicateSlug;
        }
        if db_err.is_foreign_key_violation() {
            return VoiceError::UnknownInstrument;
        }
    }
    VoiceError::Database(err)
}

/// Slugify a voice name (CLAUDE.md: slugs generated at creation, immutable
/// thereafter). Reuses the shared algorithm — see
/// [`crate::domain::user::slugify`].
pub fn slugify(input: &str) -> String {
    crate::domain::user::slugify(input)
}

/// Validate that `instrument_id` references a live `instrument` row
/// (mirrors [`crate::domain::membership::validate_fields`]'s pattern for the
/// `uuid[]` columns there, but this is a single scalar FK — Postgres *could*
/// enforce this with a real FK, and the migration already declares
/// `instrument_id uuid NOT NULL REFERENCES instrument (id)`, so the only
/// reason to also check here is to turn the bare FK-violation `23503` into a
/// clean domain error rather than a generic database error).
async fn validate_instrument_id(pool: &PgPool, instrument_id: Uuid) -> Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM instrument WHERE id = $1) as "exists!""#,
        instrument_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

struct Row {
    id: Uuid,
    arrangement_id: Uuid,
    name: String,
    slug: String,
    instrument_id: Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    deleted_at: Option<DateTime<Utc>>,
}

impl Row {
    fn into_voice(self) -> Voice {
        Voice {
            id: self.id,
            arrangement_id: self.arrangement_id,
            name: self.name,
            slug: self.slug,
            instrument_id: self.instrument_id,
            created_at: self.created_at,
            updated_at: self.updated_at,
            created_by: self.created_by,
            deleted_at: self.deleted_at,
        }
    }
}

/// Insert a new voice row. Validates `instrument_id` up front so the error
/// is a clean [`VoiceError::UnknownInstrument`] rather than a raw FK
/// violation.
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    arrangement_id: Uuid,
    name: &str,
    slug: &str,
    instrument_id: Uuid,
    created_by: Option<Uuid>,
) -> Result<Voice, VoiceError> {
    if !validate_instrument_id(pool, instrument_id).await? {
        return Err(VoiceError::UnknownInstrument);
    }

    let row = sqlx::query_as!(
        Row,
        r#"
        INSERT INTO voice (id, arrangement_id, name, slug, instrument_id, created_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING
            id, arrangement_id, name, slug, instrument_id,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        arrangement_id,
        name,
        slug,
        instrument_id,
        created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row.into_voice())
}

/// Look up a voice by id. Hides the row if it is itself soft-deleted, OR if
/// its parent Arrangement is soft-deleted (CLAUDE.md hide-with-references:
/// a soft-deleted parent hides its subtree *through it* without touching
/// the child's own `deleted_at`).
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Voice>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT
            v.id, v.arrangement_id, v.name, v.slug, v.instrument_id,
            v.created_at as "created_at: DateTime<Utc>",
            v.updated_at as "updated_at: DateTime<Utc>",
            v.created_by,
            v.deleted_at as "deleted_at: DateTime<Utc>"
        FROM voice v
        JOIN arrangement a ON a.id = v.arrangement_id
        WHERE v.id = $1 AND v.deleted_at IS NULL AND a.deleted_at IS NULL
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_voice))
}

/// Look up a voice by id regardless of its own or its parent's
/// `deleted_at` — used by undelete.
pub async fn find_by_id_including_deleted(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Voice>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT
            id, arrangement_id, name, slug, instrument_id,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM voice
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_voice))
}

/// Sort allowlist for `GET /v1/orgs/{orgId}/arrangements/{id}/voices`.
/// Default sort is `name:asc`.
// Columns are qualified with the `v.` alias because `list_for_arrangement`
// joins `arrangement a`, and both tables have `created_at` — an unqualified
// `ORDER BY created_at` is ambiguous and errors at runtime.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[("name", "v.name"), ("createdAt", "v.created_at")];

/// Filter allowlist: exact match on `instrumentId`.
pub const FILTER_ALLOWLIST: &[&str] = &["instrumentId"];

/// Fetch one page of voices for a (live) arrangement, applying the
/// hide-with-references join filter. Returns an empty page (not an error) if
/// the parent arrangement is itself soft-deleted or absent — callers that
/// need to distinguish "arrangement not found" from "no voices" should
/// resolve the arrangement separately first (the `/v1` handlers do this).
pub async fn list_for_arrangement(
    pool: &PgPool,
    arrangement_id: Uuid,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    instrument_id_filter: Option<Uuid>,
) -> Result<(Vec<Voice>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    let query = format!(
        r#"
        SELECT
            v.id, v.arrangement_id, v.name, v.slug, v.instrument_id,
            v.created_at, v.updated_at, v.created_by, v.deleted_at,
            count(*) OVER() as total
        FROM voice v
        JOIN arrangement a ON a.id = v.arrangement_id
        WHERE v.arrangement_id = $3
          AND v.deleted_at IS NULL
          AND a.deleted_at IS NULL
          AND ($4::uuid IS NULL OR v.instrument_id = $4)
        ORDER BY {sort_column} {direction}, v.id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(arrangement_id)
        .bind(instrument_id_filter)
        .fetch_all(pool)
        .await?;

    use sqlx::Row as SqlxRow;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            let count_query = r#"
                SELECT count(*) FROM voice v
                JOIN arrangement a ON a.id = v.arrangement_id
                WHERE v.arrangement_id = $1
                  AND v.deleted_at IS NULL
                  AND a.deleted_at IS NULL
                  AND ($2::uuid IS NULL OR v.instrument_id = $2)
            "#;
            sqlx::query_scalar::<_, i64>(count_query)
                .bind(arrangement_id)
                .bind(instrument_id_filter)
                .fetch_one(pool)
                .await?
        }
    };

    let items = rows
        .into_iter()
        .map(|row| {
            Ok(Voice {
                id: row.try_get("id")?,
                arrangement_id: row.try_get("arrangement_id")?,
                name: row.try_get("name")?,
                slug: row.try_get("slug")?,
                instrument_id: row.try_get("instrument_id")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
                created_by: row.try_get("created_by")?,
                deleted_at: row.try_get("deleted_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// Update a voice's mutable fields (`name`, `instrument_id`). `slug` is
/// immutable. Returns `None` if no live row (own + parent) matched `id`.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    instrument_id: Uuid,
) -> Result<Option<Voice>, VoiceError> {
    if !validate_instrument_id(pool, instrument_id).await? {
        return Err(VoiceError::UnknownInstrument);
    }

    let row = sqlx::query_as!(
        Row,
        r#"
        UPDATE voice v
        SET name = $2, instrument_id = $3, updated_at = now()
        WHERE v.id = $1 AND v.deleted_at IS NULL
        RETURNING
            v.id, v.arrangement_id, v.name, v.slug, v.instrument_id,
            v.created_at as "created_at: DateTime<Utc>",
            v.updated_at as "updated_at: DateTime<Utc>",
            v.created_by,
            v.deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        name,
        instrument_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row.map(Row::into_voice))
}

/// Soft-delete: sets `deleted_at` on this row ONLY. Returns `false` if no
/// live row matched `id`.
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE voice SET deleted_at = now(), updated_at = now() WHERE id = $1 AND deleted_at IS NULL"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Undelete: clears `deleted_at`. Returns `false` if no soft-deleted row
/// matched `id`.
pub async fn undelete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE voice SET deleted_at = NULL, updated_at = now() WHERE id = $1 AND deleted_at IS NOT NULL"#,
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
        assert_eq!(slugify("Flute 1"), "flute-1");
    }
}
