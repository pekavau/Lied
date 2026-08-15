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
    ("relevance", RELEVANCE_FRAGMENT),
];

/// The `relevance` sort fragment. `$5` is the `q` bind in [`list_for_org`]'s
/// query — the ranking is only meaningful alongside the search term, which is
/// why [`resolve_search_sort`] refuses to use it without one.
///
/// `ts_rank` first (weighted: a title hit beats an instrumentation hit), then
/// trigram similarity so a fuzzy-only match — one the tsquery could not tokenise
/// — still orders sensibly among its peers rather than tying at zero.
const RELEVANCE_FRAGMENT: &str = "(ts_rank(a.search_vector, \
     websearch_to_tsquery('simple', immutable_unaccent($5))) \
     + similarity(immutable_unaccent(a.title), immutable_unaccent($5)))";

/// Filter allowlist: `status` (exact match) plus the phase-1 `?q=` ILIKE
/// search is handled separately (not via the `filter[...]` bracket syntax —
/// CLAUDE.md: "Phase-1 ILIKE search stays a separate `?q=` param").
pub const FILTER_ALLOWLIST: &[&str] = &[
    "status",
    "difficultyMin",
    "difficultyMax",
    "durationMinSeconds",
    "durationMaxSeconds",
    "tag",
    "instrumentId",
];

/// The faceted search a caller asks for. Grouped into one parameter because
/// `list_for_org` already carried eight arguments before search existed; six
/// more positional `Option`s would be unreadable at every call site.
///
/// Every facet is optional and they AND together, composing with `q` — the
/// conductor's actual question is "something festive, brass, grade 3-5, under
/// five minutes", not any one of those alone.
#[derive(Debug, Default, Clone)]
pub struct ArrangementSearch<'a> {
    /// Free-text query: full-text over title/composer/arranger/tags/
    /// instrumentation, plus trigram fuzzy matching on title and composer.
    pub q: Option<&'a str>,
    pub status: Option<&'a str>,
    pub difficulty_min: Option<i16>,
    pub difficulty_max: Option<i16>,
    pub duration_min_seconds: Option<i32>,
    pub duration_max_seconds: Option<i32>,
    /// Tags an arrangement must carry — **all** of them (AND), consistent with
    /// how every other facet composes. The value syntax leaves room for an
    /// explicit OR later without breaking this meaning.
    pub tag_ids: Vec<Uuid>,
    /// Instrumentation: the arrangement must have a live voice for this
    /// instrument.
    pub instrument_id: Option<Uuid>,
}

/// Resolve the sort for a search, applying the two rules `relevance` needs on
/// top of the generic allowlist:
///
///  * bare `sort=relevance` means *best first*; the generic parser defaults a
///    missing direction to ascending, which would put the worst matches on
///    page one;
///  * relevance without a `q` has nothing to rank, so it falls back to the
///    default sort rather than erroring — a UI that keeps the sort while the
///    user clears the search box should not 400 at them.
pub fn resolve_search_sort<'a>(
    raw: Option<&str>,
    has_query: bool,
    default: (&'a str, crate::listing::SortDirection),
) -> Result<(&'a str, crate::listing::SortDirection), garde::Report> {
    use crate::listing::SortDirection;

    let asked_for_relevance = raw
        .map(|value| value.split(':').next().unwrap_or("") == "relevance")
        .unwrap_or(false);

    // Validate first, always: falling back early would let a malformed spec
    // like `relevance:sideways` through with a 200 and the default sort, when
    // the conventions promise a 400 for an unknown direction.
    let (fragment, direction) = crate::listing::resolve_sort(raw, SORT_ALLOWLIST, default)?;
    if asked_for_relevance && !has_query {
        return Ok(default);
    }
    let direction = if asked_for_relevance && raw.map(|v| !v.contains(':')).unwrap_or(false) {
        SortDirection::Desc
    } else {
        direction
    };
    Ok((fragment, direction))
}

/// Fetch one page of an organization's arrangements plus the total live row
/// count, with an allowlisted sort and the faceted search in
/// [`ArrangementSearch`].
///
/// `q` matches three ways, OR'd together, so one box serves both "I know what
/// it is called" and "I half-remember it":
///   * the stored `search_vector` (title, composer, arranger, tag names,
///     instrumentation) via `websearch_to_tsquery` — which, unlike
///     `to_tsquery`, accepts whatever a human types without raising, and gives
///     quoted phrases and `-exclusion` for free;
///   * trigram similarity on the unaccented title and composer, which is what
///     catches misspellings and partial words the tokeniser cannot.
///
/// Soft-deleted arrangements are always excluded.
pub async fn list_for_org(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    search: &ArrangementSearch<'_>,
) -> Result<(Vec<Arrangement>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    // The WHERE body is identical for the page and the fallback count, and is
    // the only place the facets are expressed — keeping them in one string
    // stops the two queries drifting apart as facets are added.
    const WHERE_BODY: &str = r#"
          a.deleted_at IS NULL
          AND ($4::text IS NULL OR a.status = $4)
          AND ($5::text IS NULL OR (
                  a.search_vector @@ websearch_to_tsquery('simple', immutable_unaccent($5))
               -- The `%` OPERATOR, not `similarity(...) >= threshold`: only the
               -- operator form can use the trigram GIN indexes (the function
               -- form plans as a sequential scan even with `enable_seqscan=off`,
               -- which is the whole reason those indexes exist).
               --
               -- Its cutoff therefore comes from `pg_trgm.similarity_threshold`
               -- (default 0.3) rather than from this query. That is a deliberate
               -- trade of determinism for the index: an operator who retunes the
               -- GUC changes how forgiving search is, so
               -- `fuzzy_matching_holds_at_the_default_threshold` pins the
               -- behaviour we ship with and will fail loudly if it moves.
               OR immutable_unaccent(a.title) % immutable_unaccent($5)
               OR immutable_unaccent(w.composer) % immutable_unaccent($5)
          ))
          AND ($6::smallint IS NULL OR a.difficulty >= $6)
          AND ($7::smallint IS NULL OR a.difficulty <= $7)
          AND ($8::integer IS NULL OR a.duration_seconds >= $8)
          AND ($9::integer IS NULL OR a.duration_seconds <= $9)
          AND (
              $10::uuid[] IS NULL
              OR (
                  SELECT count(DISTINCT at.tag_id)
                  FROM arrangement_tag at
                  WHERE at.arrangement_id = a.id AND at.tag_id = ANY($10)
              ) = cardinality($10)
          )
          AND (
              $11::uuid IS NULL
              OR EXISTS (
                  SELECT 1 FROM voice v
                  WHERE v.arrangement_id = a.id
                    AND v.instrument_id = $11
                    AND v.deleted_at IS NULL
              )
          )
    "#;

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
        WHERE a.organization_id = $3 AND {WHERE_BODY}
        ORDER BY {sort_column} {direction}, a.id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    // An empty tag list means "no tag facet", not "must carry zero tags".
    let tag_ids: Option<&[Uuid]> =
        (!search.tag_ids.is_empty()).then_some(search.tag_ids.as_slice());

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(organization_id)
        .bind(search.status)
        .bind(search.q)
        .bind(search.difficulty_min)
        .bind(search.difficulty_max)
        .bind(search.duration_min_seconds)
        .bind(search.duration_max_seconds)
        .bind(tag_ids)
        .bind(search.instrument_id)
        .fetch_all(pool)
        .await?;

    use sqlx::Row as SqlxRow;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            // The shared WHERE body numbers its binds from $3 (the page query
            // spends $1/$2 on LIMIT/OFFSET). Rather than bind two values this
            // statement never references, keep the positions and let the two
            // leading binds be explicit placeholders — NULL costs nothing and
            // makes the shared numbering visible instead of implied.
            let count_query = format!(
                r#"
                SELECT count(*)
                FROM arrangement a
                LEFT JOIN work w ON w.id = a.work_id
                WHERE a.organization_id = $3 AND {WHERE_BODY}
                "#
            );
            sqlx::query_scalar::<_, i64>(&count_query)
                .bind(None::<i64>)
                .bind(None::<i64>)
                .bind(organization_id)
                .bind(search.status)
                .bind(search.q)
                .bind(search.difficulty_min)
                .bind(search.difficulty_max)
                .bind(search.duration_min_seconds)
                .bind(search.duration_max_seconds)
                .bind(tag_ids)
                .bind(search.instrument_id)
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
