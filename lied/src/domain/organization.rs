//! Organization: an orchestra or ensemble (CLAUDE.md Entities: Organization).
//!
//! Issue #5 (Org/User/Membership management) adds full CRUD: create/delete
//! are system-admin-gated instance operations (no `deleted_at` — deletion is
//! an admin-gated hard delete per CLAUDE.md); list/get are open to any
//! authenticated user (browsing the org directory is low-sensitivity).
//! Update is limited to `name` in phase 1 — `slug` is immutable per the
//! WebDAV-layout decision and renaming is a separate, explicit, audited op
//! deferred to a later item.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of an `organization` row. `camelCase` per CLAUDE.md's
/// JSON-casing convention.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Organization {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(thiserror::Error, Debug)]
pub enum OrganizationError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("slug already exists")]
    DuplicateSlug,
}

fn map_insert_error(err: sqlx::Error) -> OrganizationError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("organization_slug_key") {
            return OrganizationError::DuplicateSlug;
        }
    }
    OrganizationError::Database(err)
}

/// Slugify an organization name into the ASCII, lowercase, hyphen-separated
/// form used for `Organization.slug` (immutable; see [`crate::domain::user::slugify`]
/// for the identical algorithm — kept duplicated rather than shared since the
/// two entities' slug rules could diverge later, e.g. organization slugs
/// disallowing certain reserved words).
pub fn slugify(input: &str) -> String {
    crate::domain::user::slugify(input)
}

/// Insert a new organization row.
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    slug: &str,
    created_by: Option<Uuid>,
) -> Result<Organization, OrganizationError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO organization (id, name, slug, created_by)
        VALUES ($1, $2, $3, $4)
        RETURNING
            id, name, slug,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        "#,
        id,
        name,
        slug,
        created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_insert_error)?;

    Ok(Organization {
        id: row.id,
        name: row.name,
        slug: row.slug,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Look up an organization by id.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Organization>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT
            id, name, slug,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        FROM organization
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| Organization {
        id: row.id,
        name: row.name,
        slug: row.slug,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

/// Sort allowlist for `GET /v1/orgs` (CLAUDE.md "Sort & filter": per-endpoint
/// allowlist). Default sort is `name:asc`.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[
    ("name", "name"),
    ("slug", "slug"),
    ("createdAt", "created_at"),
];

/// Filter allowlist for `GET /v1/orgs`: `?filter[name]=` does a case-insensitive
/// substring match (phase-1 ILIKE search, per CLAUDE.md's basic-search scope).
pub const FILTER_ALLOWLIST: &[&str] = &["name"];

/// Fetch one page of organizations plus the total row count, with an
/// allowlisted sort column/direction and an allowlisted `name` ILIKE filter.
/// `sort_column`/`sort_direction` come from [`crate::listing::resolve_sort`]
/// against [`SORT_ALLOWLIST`] — by the time this function runs the values are
/// trusted fixed SQL fragments, never raw user input, so building the query
/// string with `format!` here does not reopen the SQL-injection surface the
/// allowlist closes.
pub async fn list(
    pool: &PgPool,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    name_filter: Option<&str>,
) -> Result<(Vec<Organization>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    // `sort_column`/`direction` are always one of the fixed fragments in
    // SORT_ALLOWLIST/SortDirection::as_sql, never raw input (see allowlist
    // note above) — `id` is appended as a stable tiebreaker since `name`/
    // `slug`/`created_at` are not guaranteed unique.
    // This is a runtime `sqlx::query` (the ORDER BY is built from allowlisted
    // fragments, so the compile-time macro can't be used). The column aliases
    // here are therefore plain SQL identifiers — NOT the `"name!: Type"`
    // cast-annotation syntax, which is a `query!`-macro-only feature and would
    // be sent verbatim to Postgres as a malformed identifier.
    let query = format!(
        r#"
        SELECT
            id, name, slug,
            created_at,
            updated_at,
            count(*) OVER() as total
        FROM organization
        WHERE ($3::text IS NULL OR name ILIKE '%' || $3 || '%')
        ORDER BY {sort_column} {direction}, id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(name_filter)
        .fetch_all(pool)
        .await?;

    use sqlx::Row;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            let count_query = "SELECT count(*) FROM organization WHERE ($1::text IS NULL OR name ILIKE '%' || $1 || '%')";
            sqlx::query_scalar::<_, i64>(count_query)
                .bind(name_filter)
                .fetch_one(pool)
                .await?
        }
    };

    let items = rows
        .into_iter()
        .map(|row| {
            Ok(Organization {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                slug: row.try_get("slug")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// Update an organization's `name`. `slug` is immutable (see module doc).
/// Returns the updated row, or `None` if no row matched `id`.
pub async fn update_name(
    pool: &PgPool,
    id: Uuid,
    name: &str,
) -> Result<Option<Organization>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        UPDATE organization
        SET name = $2, updated_at = now()
        WHERE id = $1
        RETURNING
            id, name, slug,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        "#,
        id,
        name,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| Organization {
        id: row.id,
        name: row.name,
        slug: row.slug,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

/// Hard-delete an organization, cascading in the FK-safe order documented in
/// CLAUDE.md ("Cascade order"): PartAssignments -> CollectionItems ->
/// Collections -> ArrangementTags -> Tags -> GlobalAnnotations -> Files ->
/// Voices -> Arrangements -> Memberships -> the Organization row. Phase 1 has
/// no File/Voice/Arrangement/Collection/etc. domain modules yet (later
/// items), but the *tables* already exist (issue #3 migrations), so this
/// function deletes from all of them now — leaving any out would silently
/// leave orphaned rows the moment those entities are populated by later
/// items, which would be a much harder bug to find than a few `DELETE`s
/// against currently-empty tables today.
///
/// Runs in a single transaction: either the whole cascade commits or none of
/// it does. MinIO objects are not touched here — file/object cleanup for a
/// hard-deleted org is a phase-2 concern once the File domain module (and its
/// S3 key derivation) exists; flagged here rather than silently doing nothing.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query!(
        r#"
        DELETE FROM part_assignment
        WHERE collection_item_id IN (
            SELECT ci.id FROM collection_item ci
            JOIN collection c ON c.id = ci.collection_id
            WHERE c.organization_id = $1
        )
        "#,
        id
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        r#"
        DELETE FROM collection_item
        WHERE collection_id IN (SELECT id FROM collection WHERE organization_id = $1)
        "#,
        id
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(r#"DELETE FROM collection WHERE organization_id = $1"#, id)
        .execute(&mut *tx)
        .await?;

    sqlx::query!(
        r#"
        DELETE FROM arrangement_tag
        WHERE tag_id IN (SELECT id FROM tag WHERE organization_id = $1)
           OR arrangement_id IN (SELECT id FROM arrangement WHERE organization_id = $1)
        "#,
        id
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(r#"DELETE FROM tag WHERE organization_id = $1"#, id)
        .execute(&mut *tx)
        .await?;

    sqlx::query!(
        r#"
        DELETE FROM global_annotation
        WHERE arrangement_id IN (SELECT id FROM arrangement WHERE organization_id = $1)
        "#,
        id
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        r#"
        DELETE FROM file
        WHERE arrangement_id IN (SELECT id FROM arrangement WHERE organization_id = $1)
        "#,
        id
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        r#"
        DELETE FROM voice
        WHERE arrangement_id IN (SELECT id FROM arrangement WHERE organization_id = $1)
        "#,
        id
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(r#"DELETE FROM arrangement WHERE organization_id = $1"#, id)
        .execute(&mut *tx)
        .await?;

    sqlx::query!(r#"DELETE FROM membership WHERE organization_id = $1"#, id)
        .execute(&mut *tx)
        .await?;

    let result = sqlx::query!(r#"DELETE FROM organization WHERE id = $1"#, id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_matches_user_slugify() {
        assert_eq!(slugify("Vienna Philharmonic"), "vienna-philharmonic");
    }
}
