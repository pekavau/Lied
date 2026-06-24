//! User: a person using the system (CLAUDE.md Entities: User).
//!
//! Phase-1 item 4 (auth) needs enough of this entity to bootstrap an admin,
//! log in, and look up a user by username/id. Full CRUD (admin UI) lands in
//! a later item.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of a `user` row. `password_hash` is intentionally
/// never included — this type must never leak it via any `/v1` response.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: Uuid,
    pub slug: String,
    pub username: String,
    pub email: Option<String>,
    pub display_name: String,
    pub is_system_admin: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Internal row shape used only for auth checks (login, bearer/session
/// resolution) — carries `password_hash`, which [`User`] deliberately omits.
/// Never serialized; never returned from a handler.
#[derive(Debug, Clone)]
pub struct UserWithHash {
    pub id: Uuid,
    pub slug: String,
    pub username: String,
    pub email: Option<String>,
    pub display_name: String,
    pub password_hash: Option<String>,
    pub is_system_admin: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(thiserror::Error, Debug)]
pub enum UserError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("username already exists")]
    DuplicateUsername,
}

fn map_insert_error(err: sqlx::Error) -> UserError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("user_username_key") {
            return UserError::DuplicateUsername;
        }
    }
    UserError::Database(err)
}

/// Slugify a username into the ASCII, lowercase, hyphen-separated form used
/// for `User.slug` (immutable; distinct from the renameable `username`).
/// Any character that isn't an ASCII alphanumeric becomes a hyphen; runs of
/// hyphens collapse to one, and leading/trailing hyphens are trimmed.
pub fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    let mut last_was_hyphen = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_hyphen = false;
        } else if !last_was_hyphen && !slug.is_empty() {
            slug.push('-');
            last_was_hyphen = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("user");
    }
    slug
}

/// Insert a new user row. `password_hash` is `None` for OIDC-only accounts
/// (not used in phase 1, but the column allows it).
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    slug: &str,
    username: &str,
    email: Option<&str>,
    password_hash: Option<&str>,
    display_name: &str,
    is_system_admin: bool,
    created_by: Option<Uuid>,
) -> Result<User, UserError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO "user" (id, slug, username, email, password_hash, display_name, is_system_admin, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING
            id, slug, username::text as "username!", email::text as "email",
            display_name, is_system_admin,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        "#,
        id,
        slug,
        username,
        email,
        password_hash,
        display_name,
        is_system_admin,
        created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_insert_error)?;

    Ok(User {
        id: row.id,
        slug: row.slug,
        username: row.username,
        email: row.email,
        display_name: row.display_name,
        is_system_admin: row.is_system_admin,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Look up a user (with password hash) by username, case-insensitively
/// (the column is `citext`). Used by the login flow.
pub async fn find_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<UserWithHash>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT
            id, slug, username::text as "username!", email::text as "email",
            password_hash, display_name, is_system_admin,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        FROM "user"
        WHERE username = $1
        "#,
        username,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| UserWithHash {
        id: row.id,
        slug: row.slug,
        username: row.username,
        email: row.email,
        display_name: row.display_name,
        password_hash: row.password_hash,
        is_system_admin: row.is_system_admin,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

/// Look up a user by id. Used by session/bearer-token resolution.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<User>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT
            id, slug, username::text as "username!", email::text as "email",
            display_name, is_system_admin,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        FROM "user"
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| User {
        id: row.id,
        slug: row.slug,
        username: row.username,
        email: row.email,
        display_name: row.display_name,
        is_system_admin: row.is_system_admin,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

/// Sort allowlist for `GET /v1/users`. Default sort is `username:asc`.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[
    ("username", "username"),
    ("displayName", "display_name"),
    ("createdAt", "created_at"),
];

/// Filter allowlist for `GET /v1/users`: `?filter[username]=` is a
/// case-insensitive substring match (the column is `citext`, so a plain
/// `LIKE` is already case-insensitive; `ILIKE` is used for parity with the
/// other ILIKE filters in this codebase).
pub const FILTER_ALLOWLIST: &[&str] = &["username"];

/// Fetch one page of users plus the total row count, with an allowlisted
/// sort and an optional `username` substring filter. System-admin-only
/// endpoint (CLAUDE.md: instance-level user management); see
/// [`crate::domain::organization::list`] for why the `format!`-built query
/// around fixed, allowlisted fragments is safe.
pub async fn list(
    pool: &PgPool,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    username_filter: Option<&str>,
) -> Result<(Vec<User>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    // Runtime `sqlx::query` (dynamic allowlisted ORDER BY): aliases are plain
    // SQL identifiers, not the `"name!: Type"` cast-annotation syntax (a
    // `query!`-macro-only feature Postgres would reject). The `::text` casts on
    // the citext columns remain — those are real SQL, needed so `try_get::<String>`
    // can decode them.
    let query = format!(
        r#"
        SELECT
            id, slug, username::text as username, email::text as email,
            display_name, is_system_admin,
            created_at,
            updated_at,
            count(*) OVER() as total
        FROM "user"
        WHERE ($3::text IS NULL OR username ILIKE '%' || $3 || '%')
        ORDER BY {sort_column} {direction}, id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(username_filter)
        .fetch_all(pool)
        .await?;

    use sqlx::Row;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            let count_query = r#"SELECT count(*) FROM "user" WHERE ($1::text IS NULL OR username ILIKE '%' || $1 || '%')"#;
            sqlx::query_scalar::<_, i64>(count_query)
                .bind(username_filter)
                .fetch_one(pool)
                .await?
        }
    };

    let items = rows
        .into_iter()
        .map(|row| {
            Ok(User {
                id: row.try_get("id")?,
                slug: row.try_get("slug")?,
                username: row.try_get("username")?,
                email: row.try_get("email")?,
                display_name: row.try_get("display_name")?,
                is_system_admin: row.try_get("is_system_admin")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// Hard-delete a user (CLAUDE.md: "user deletion is admin-gated hard delete
/// with explicit reassignment-or-anonymization of PartAssignments,
/// GlobalAnnotations, and audit log entries" — full reassignment/
/// anonymization flows are a phase-2 concern per the entity doc, but this
/// item still must not leave a dangling FK or silently 500). To keep the
/// delete itself safe today without a full reassignment UI, the rows this
/// user is referenced from are nulled/cleared rather than cascaded:
/// - `membership.created_by`, `organization.created_by`, and other
///   `created_by` audit columns are nullable and simply keep pointing at a
///   (now-gone) id only via `ON DELETE` behavior already declared on the
///   FK — *except* `membership.user_id`, `part_assignment.user_id`, and
///   `global_annotation.author_id`, which are `NOT NULL` and have no
///   `ON DELETE` clause, so a delete while those rows exist would violate
///   the FK and error. Phase 1 accepts this as a hard stop (the caller sees
///   a clear DB error) rather than silently deleting a user who still has
///   active memberships/assignments/annotations — an explicit
///   reassignment flow is the correct fix and is deferred, per CLAUDE.md.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(r#"DELETE FROM "user" WHERE id = $1"#, id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Returns `true` if any row in `"user"` has `is_system_admin = true`. Used
/// by `create-admin` only to log an informational note when bootstrapping a
/// second admin (not a hard guard — operators may legitimately want more
/// than one system admin).
pub async fn any_system_admin_exists(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM "user" WHERE is_system_admin) as "exists!""#
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_lowercases_and_hyphenates() {
        assert_eq!(slugify("Alice Archivist"), "alice-archivist");
    }

    #[test]
    fn slugify_collapses_runs_and_trims_edges() {
        assert_eq!(slugify("  weird___name!!"), "weird-name");
    }

    #[test]
    fn slugify_falls_back_when_empty() {
        assert_eq!(slugify("!!!"), "user");
    }

    #[test]
    fn slugify_handles_already_clean_input() {
        assert_eq!(slugify("admin"), "admin");
    }
}
