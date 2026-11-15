//! Voice: an individual instrument part within an arrangement (docs/design.md
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

/// Wire representation of a `voice` row. `camelCase` per docs/design.md's JSON-
/// casing convention.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Voice {
    pub id: Uuid,
    pub arrangement_id: Uuid,
    pub name: String,
    pub slug: String,
    /// The instruments this part is read on, in order; never empty. The first
    /// is the **primary**: "Flute 2 / Piccolo" is `[flute, piccolo]`, and a
    /// percussion part lists every instrument its player handles (#57).
    /// Display, sorting and part identity use the primary.
    pub instrument_ids: Vec<Uuid>,
    /// Which part of its instrument this voice is — the `1` of "Trumpet in Bb
    /// 1". `None` is an ordinary value (Timpani, Harp, Solo Cornet), not a
    /// missing one. `(primary instrument, part_number)` is how "the same
    /// part" is recognised across pieces (issue #55).
    pub part_number: Option<i16>,
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
    #[error("instrumentIds must reference live instruments")]
    UnknownInstrument,
    #[error("instrumentIds must name at least one instrument")]
    NoInstrument,
    #[error("instrumentIds must not name an instrument twice")]
    DuplicateInstrument,
    #[error("part_number must be at least 1")]
    InvalidPartNumber,
}

/// The part number a new voice gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewPartNumber {
    /// Nobody said: guess it from the name with the same
    /// `lied_guess_part_number` the backfill used (migration 0024), so voices
    /// created later number the same way as the ones that existed before.
    /// "Trumpet in Bb 1" gets 1; "Timpani" or "Horn 1 & 3" get none.
    FromName,
    /// Exactly this — `None` is an explicit "this part has no number".
    Set(Option<i16>),
}

/// What an update does to a voice's part number. `Keep` matters: a client
/// that does not know about part numbers must not erase one by omitting it.
pub type PartNumberChange = super::Change<i16>;

/// The rule the `voice.part_number` CHECK enforces, applied up front so a bad
/// value is a clean domain error rather than a constraint violation.
fn validate_part_number(part_number: Option<i16>) -> Result<(), VoiceError> {
    match part_number {
        Some(n) if n < 1 => Err(VoiceError::InvalidPartNumber),
        _ => Ok(()),
    }
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

/// Slugify a voice name (docs/design.md: slugs generated at creation, immutable
/// thereafter). Reuses the shared algorithm — see
/// [`crate::slug::slugify`].
pub fn slugify(input: &str) -> String {
    crate::slug::slugify(input, "voice")
}

impl Voice {
    /// The primary instrument: the first of [`Voice::instrument_ids`].
    pub fn primary_instrument_id(&self) -> Uuid {
        self.instrument_ids.first().copied().unwrap_or_default()
    }
}

/// Validate a voice's instrument list: at least one, none twice, all known.
/// The table's foreign key would refuse an unknown id too; checking first
/// turns that into a clean domain error. Runs in the write's transaction,
/// which is what holds the "at least one instrument" rule: the schema
/// cannot, because the rows live in `voice_instrument`.
async fn validate_instruments(
    db: impl sqlx::PgExecutor<'_>,
    instrument_ids: &[Uuid],
) -> Result<(), VoiceError> {
    if instrument_ids.is_empty() {
        return Err(VoiceError::NoInstrument);
    }
    let mut distinct = instrument_ids.to_vec();
    distinct.sort();
    distinct.dedup();
    if distinct.len() != instrument_ids.len() {
        return Err(VoiceError::DuplicateInstrument);
    }
    let known: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "count!" FROM instrument WHERE id = ANY($1)"#,
        &distinct,
    )
    .fetch_one(db)
    .await?;
    if known != distinct.len() as i64 {
        return Err(VoiceError::UnknownInstrument);
    }
    Ok(())
}

/// Replace a voice's instrument rows with `instrument_ids`, in order.
async fn write_instruments(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    voice_id: Uuid,
    instrument_ids: &[Uuid],
) -> Result<(), VoiceError> {
    sqlx::query!("DELETE FROM voice_instrument WHERE voice_id = $1", voice_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query!(
        r#"
        INSERT INTO voice_instrument (voice_id, instrument_id, position)
        SELECT $1, i.id, i.ord::smallint
        FROM unnest($2::uuid[]) WITH ORDINALITY AS i(id, ord)
        "#,
        voice_id,
        instrument_ids,
    )
    .execute(&mut **tx)
    .await
    .map_err(map_write_error)?;
    Ok(())
}

struct Row {
    id: Uuid,
    arrangement_id: Uuid,
    name: String,
    slug: String,
    instrument_ids: Vec<Uuid>,
    part_number: Option<i16>,
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
            instrument_ids: self.instrument_ids,
            part_number: self.part_number,
            created_at: self.created_at,
            updated_at: self.updated_at,
            created_by: self.created_by,
            deleted_at: self.deleted_at,
        }
    }
}

/// Insert a new voice with its instruments (first = primary), in one
/// transaction. The list is validated first ([`VoiceError::NoInstrument`],
/// [`VoiceError::DuplicateInstrument`], [`VoiceError::UnknownInstrument`]).
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    arrangement_id: Uuid,
    name: &str,
    slug: &str,
    instrument_ids: &[Uuid],
    part_number: NewPartNumber,
    created_by: Option<Uuid>,
) -> Result<Voice, VoiceError> {
    let (from_name, explicit) = match part_number {
        NewPartNumber::FromName => (true, None),
        NewPartNumber::Set(n) => (false, n),
    };
    validate_part_number(explicit)?;
    let mut tx = pool.begin().await?;
    validate_instruments(&mut *tx, instrument_ids).await?;

    let row = sqlx::query!(
        r#"
        INSERT INTO voice (id, arrangement_id, name, slug, part_number, created_by)
        VALUES (
            $1, $2, $3, $4,
            CASE WHEN $7 THEN lied_guess_part_number($3) ELSE $5 END,
            $6
        )
        RETURNING
            id, arrangement_id, name, slug, part_number,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        arrangement_id,
        name,
        slug,
        explicit,
        created_by,
        from_name,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(map_write_error)?;
    write_instruments(&mut tx, id, instrument_ids).await?;
    tx.commit().await?;

    Ok(Voice {
        id: row.id,
        arrangement_id: row.arrangement_id,
        name: row.name,
        slug: row.slug,
        instrument_ids: instrument_ids.to_vec(),
        part_number: row.part_number,
        created_at: row.created_at,
        updated_at: row.updated_at,
        created_by: row.created_by,
        deleted_at: row.deleted_at,
    })
}

/// Look up a voice by id. Hides the row if it is itself soft-deleted, OR if
/// its parent Arrangement is soft-deleted (docs/design.md hide-with-references:
/// a soft-deleted parent hides its subtree *through it* without touching
/// the child's own `deleted_at`).
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Voice>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT
            v.id, v.arrangement_id, v.name, v.slug,
            ARRAY(SELECT vi.instrument_id FROM voice_instrument vi WHERE vi.voice_id = v.id ORDER BY vi.position) as "instrument_ids!",
            v.part_number,
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
            v.id, v.arrangement_id, v.name, v.slug,
            ARRAY(SELECT vi.instrument_id FROM voice_instrument vi WHERE vi.voice_id = v.id ORDER BY vi.position) as "instrument_ids!",
            v.part_number,
            v.created_at as "created_at: DateTime<Utc>",
            v.updated_at as "updated_at: DateTime<Utc>",
            v.created_by,
            v.deleted_at as "deleted_at: DateTime<Utc>"
        FROM voice v
        WHERE v.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_voice))
}

/// Sort allowlist for `GET /v1/orgs/{orgId}/arrangements/{arrangementId}/voices`.
/// Default sort is `name:asc`.
// Columns are qualified with the `v.` alias because `list_for_arrangement`
// joins `arrangement a`, and both tables have `created_at` — an unqualified
// `ORDER BY created_at` is ambiguous and errors at runtime.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[("name", "v.name"), ("createdAt", "v.created_at")];

/// The sort a list uses when the request names none: (SQL column from
/// [`SORT_ALLOWLIST`], direction). The OpenAPI description must name it;
/// `sort_and_filter_docs_match_the_allowlists` checks that it does.
pub const DEFAULT_SORT: (&str, crate::listing::SortDirection) =
    ("v.name", crate::listing::SortDirection::Asc);

/// Filter allowlist: `instrumentId` matches a voice that carries that
/// instrument at any position.
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
            v.id, v.arrangement_id, v.name, v.slug,
            ARRAY(SELECT vi.instrument_id FROM voice_instrument vi
                  WHERE vi.voice_id = v.id ORDER BY vi.position) as instrument_ids,
            v.part_number,
            v.created_at, v.updated_at, v.created_by, v.deleted_at,
            count(*) OVER() as total
        FROM voice v
        JOIN arrangement a ON a.id = v.arrangement_id
        WHERE v.arrangement_id = $3
          AND v.deleted_at IS NULL
          AND a.deleted_at IS NULL
          AND ($4::uuid IS NULL OR EXISTS (
                SELECT 1 FROM voice_instrument vi
                WHERE vi.voice_id = v.id AND vi.instrument_id = $4))
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
                  AND ($2::uuid IS NULL OR EXISTS (
                        SELECT 1 FROM voice_instrument vi
                        WHERE vi.voice_id = v.id AND vi.instrument_id = $2))
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
                instrument_ids: row.try_get("instrument_ids")?,
                part_number: row.try_get("part_number")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
                created_by: row.try_get("created_by")?,
                deleted_at: row.try_get("deleted_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// Update a voice's mutable fields (`name`, the instrument list,
/// `part_number`); the instrument list is replaced as a whole.
/// `slug` is immutable. Returns `None` if no live row (own + parent) matched `id`.
///
/// `expected_version`: see [guarded writes](crate::domain#guarded-writes).
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    expected_version: Option<i64>,
    name: &str,
    instrument_ids: &[Uuid],
    part_number: PartNumberChange,
) -> Result<Option<Voice>, VoiceError> {
    let (keep, explicit) = match part_number {
        PartNumberChange::Keep => (true, None),
        PartNumberChange::Set(n) => (false, n),
    };
    validate_part_number(explicit)?;
    let mut tx = pool.begin().await?;
    validate_instruments(&mut *tx, instrument_ids).await?;

    let row = sqlx::query!(
        r#"
        UPDATE voice v
        SET name = $2,
            part_number = CASE WHEN $4 THEN v.part_number ELSE $3 END,
            updated_at = now()
        WHERE v.id = $1 AND v.deleted_at IS NULL
          AND ($5::bigint IS NULL OR floor(extract(epoch FROM v.updated_at) * 1000)::bigint = $5)
        RETURNING
            v.id, v.arrangement_id, v.name, v.slug, v.part_number,
            v.created_at as "created_at: DateTime<Utc>",
            v.updated_at as "updated_at: DateTime<Utc>",
            v.created_by,
            v.deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        name,
        explicit,
        keep,
        expected_version,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_write_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    write_instruments(&mut tx, id, instrument_ids).await?;
    tx.commit().await?;

    Ok(Some(Voice {
        id: row.id,
        arrangement_id: row.arrangement_id,
        name: row.name,
        slug: row.slug,
        instrument_ids: instrument_ids.to_vec(),
        part_number: row.part_number,
        created_at: row.created_at,
        updated_at: row.updated_at,
        created_by: row.created_by,
        deleted_at: row.deleted_at,
    }))
}

/// Soft-delete: sets `deleted_at` on this row ONLY. Returns `false` if no
/// live row matched `id`.
///
/// `expected_version`: see [guarded writes](crate::domain#guarded-writes).
pub async fn soft_delete(
    pool: &PgPool,
    id: Uuid,
    expected_version: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"UPDATE voice SET deleted_at = now(), updated_at = now() WHERE id = $1 AND deleted_at IS NULL AND ($2::bigint IS NULL OR floor(extract(epoch FROM updated_at) * 1000)::bigint = $2)"#,
        id,
        expected_version,
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

    #[test]
    fn part_number_must_be_positive_or_absent() {
        assert!(validate_part_number(None).is_ok());
        assert!(validate_part_number(Some(1)).is_ok());
        assert!(matches!(
            validate_part_number(Some(0)),
            Err(VoiceError::InvalidPartNumber)
        ));
        assert!(matches!(
            validate_part_number(Some(-2)),
            Err(VoiceError::InvalidPartNumber)
        ));
    }
}
