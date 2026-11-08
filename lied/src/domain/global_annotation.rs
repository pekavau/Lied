//! GlobalAnnotation: a conductor's note on an arrangement that applies to the
//! whole ensemble — a tempo decision, a cut, a dynamic shape (CLAUDE.md
//! Entities: GlobalAnnotation; UC-16). Issue #36 activates the authoring half;
//! musicians read them in the phase-3 client, not over WebDAV.
//!
//! **Hard delete.** `global_annotation` has no `deleted_at` (CLAUDE.md
//! Unique-constraints table), so [`delete`] removes the row and returns what it
//! removed; the caller records that in the audit log, which is then the only
//! copy.
//!
//! **Scoped through the arrangement.** An annotation has no `organization_id`;
//! its org is its arrangement's. Every read *and write* here requires the
//! arrangement to be live, so a soft-deleted arrangement hides its annotations
//! (hide-with-references) without touching them, and restoring the arrangement
//! brings them back — none can be edited or deleted while hidden.
//!
//! **Optimistic concurrency is enforced in the write itself.** [`update`] and
//! [`delete`] take the version (the ETag's millisecond `updated_at`) the caller
//! saw and put it in the statement's `WHERE` clause, so two writers holding the
//! same version cannot both succeed: the second gets [`WriteError::Stale`].
//! Checking a previously-read row and then writing by id alone would let both
//! through.
//!
//! **Each write is one statement** that also returns the joined row (author
//! name included). There is no separate read-back after commit, so the caller
//! always gets the row it wrote and can always audit it.
//!
//! **Who may write** is not decided here: the permission matrix (owner and
//! conductor author; staff read) is enforced by the callers through
//! [`crate::domain::membership::Role::can_author_global_annotations`].

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// The annotation vocabulary (migration 0025's CHECK). Order is display order.
pub const TYPES: &[&str] = &["tempo", "dynamics", "articulation", "structure", "general"];

pub fn is_valid_type(value: &str) -> bool {
    TYPES.contains(&value)
}

/// "type must be one of …", built from [`TYPES`] so adding a type cannot
/// leave error messages listing the old set.
pub fn invalid_type_message() -> String {
    format!("type must be one of {}", TYPES.join(", "))
}

/// Wire representation of an annotation, with its author's display name so a
/// list can be rendered without a lookup per row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GlobalAnnotation {
    pub id: Uuid,
    pub arrangement_id: Uuid,
    pub author_id: Uuid,
    pub author_display_name: String,
    /// One of `tempo`, `dynamics`, `articulation`, `structure`, `general`.
    #[serde(rename = "type")]
    pub annotation_type: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(thiserror::Error, Debug)]
pub enum GlobalAnnotationError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("type is not one of the annotation types")]
    InvalidType,
    #[error("content must not be empty")]
    EmptyContent,
    /// The arrangement is gone (or was soft-deleted) by the time of the write.
    #[error("the arrangement does not exist")]
    ArrangementGone,
}

/// Why an [`update`] or [`delete`] wrote nothing.
#[derive(thiserror::Error, Debug)]
pub enum WriteError {
    #[error(transparent)]
    Invalid(#[from] GlobalAnnotationError),
    /// The annotation changed since the caller's version: someone else wrote
    /// first. Nothing was written.
    #[error("the annotation changed since it was read")]
    Stale,
    /// No live annotation by that id (deleted, or its arrangement removed).
    #[error("no such annotation")]
    NotFound,
}

impl From<sqlx::Error> for WriteError {
    fn from(err: sqlx::Error) -> Self {
        WriteError::Invalid(GlobalAnnotationError::Database(err))
    }
}

fn validate(
    annotation_type: Option<&str>,
    content: Option<&str>,
) -> Result<(), GlobalAnnotationError> {
    if annotation_type.is_some_and(|t| !is_valid_type(t)) {
        return Err(GlobalAnnotationError::InvalidType);
    }
    if content.is_some_and(|c| c.trim().is_empty()) {
        return Err(GlobalAnnotationError::EmptyContent);
    }
    Ok(())
}

/// Add an annotation to a live arrangement, in one statement: the INSERT
/// selects from the live arrangement row (an arrangement removed in the
/// meantime yields [`GlobalAnnotationError::ArrangementGone`] and no row) and
/// the result is joined to its author in the same statement.
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    arrangement_id: Uuid,
    author_id: Uuid,
    annotation_type: &str,
    content: &str,
) -> Result<GlobalAnnotation, GlobalAnnotationError> {
    validate(Some(annotation_type), Some(content))?;
    sqlx::query_as!(
        GlobalAnnotation,
        r#"
        WITH inserted AS (
            INSERT INTO global_annotation (id, arrangement_id, author_id, type, content, created_by)
            SELECT $1, a.id, $3, $4, $5, $3
            FROM arrangement a
            WHERE a.id = $2 AND a.deleted_at IS NULL
            RETURNING *
        )
        SELECT
            g.id as "id!", g.arrangement_id as "arrangement_id!", g.author_id as "author_id!",
            u.display_name as "author_display_name!",
            g.type as "annotation_type!", g.content as "content!",
            g.created_at as "created_at!: DateTime<Utc>",
            g.updated_at as "updated_at!: DateTime<Utc>"
        FROM inserted g
        JOIN "user" u ON u.id = g.author_id
        "#,
        id,
        arrangement_id,
        author_id,
        annotation_type,
        content,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(GlobalAnnotationError::ArrangementGone)
}

/// An annotation of `arrangement_id`, which must be a live arrangement of
/// `organization_id`. Any mismatch is `None`: an id from another org or another
/// arrangement must not resolve (the nested-scope lesson from #6).
pub async fn find(
    pool: &PgPool,
    organization_id: Uuid,
    arrangement_id: Uuid,
    id: Uuid,
) -> Result<Option<GlobalAnnotation>, sqlx::Error> {
    sqlx::query_as!(
        GlobalAnnotation,
        r#"
        SELECT
            g.id, g.arrangement_id, g.author_id,
            u.display_name as author_display_name,
            g.type as annotation_type, g.content,
            g.created_at as "created_at: DateTime<Utc>",
            g.updated_at as "updated_at: DateTime<Utc>"
        FROM global_annotation g
        JOIN arrangement a ON a.id = g.arrangement_id AND a.deleted_at IS NULL
        JOIN "user" u ON u.id = g.author_id
        WHERE g.id = $1 AND g.arrangement_id = $2 AND a.organization_id = $3
        "#,
        id,
        arrangement_id,
        organization_id,
    )
    .fetch_optional(pool)
    .await
}

/// Whether a live annotation with this id exists at all — how a write that
/// matched nothing tells "someone else wrote first" from "it is gone".
async fn exists_live(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM global_annotation g
            JOIN arrangement a ON a.id = g.arrangement_id AND a.deleted_at IS NULL
            WHERE g.id = $1
        ) as "exists!"
        "#,
        id,
    )
    .fetch_one(pool)
    .await
}

/// What an annotation list is ordered by — a closed set, so no caller can pass
/// a column name or other SQL into the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    CreatedAt,
    UpdatedAt,
    Type,
}

impl SortField {
    fn as_key(self) -> &'static str {
        match self {
            SortField::CreatedAt => "createdAt",
            SortField::UpdatedAt => "updatedAt",
            SortField::Type => "type",
        }
    }

    /// The field named by an allowlisted wire key (see [`SORT_ALLOWLIST`]).
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "createdAt" => Some(SortField::CreatedAt),
            "updatedAt" => Some(SortField::UpdatedAt),
            "type" => Some(SortField::Type),
            _ => None,
        }
    }
}

/// Sort allowlist for the `/v1` list, in `listing::resolve_sort`'s
/// `(wire field, resolved value)` shape. The resolved value is a
/// [`SortField`] key, not SQL — the query orders by a fixed `CASE` per field.
/// Default `createdAt:asc`: the order the conductor wrote them, which is
/// usually rehearsal order.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[
    ("createdAt", "createdAt"),
    ("updatedAt", "updatedAt"),
    ("type", "type"),
];

/// Filter allowlist: exact match on `type`.
pub const FILTER_ALLOWLIST: &[&str] = &["type"];

/// One page of an arrangement's annotations. The arrangement is not checked
/// here; callers resolve it (and its org) first, so "no such arrangement" and
/// "no annotations" stay distinguishable.
pub async fn list_for_arrangement(
    pool: &PgPool,
    arrangement_id: Uuid,
    limit: i64,
    offset: i64,
    sort: SortField,
    direction: crate::listing::SortDirection,
    type_filter: Option<&str>,
) -> Result<(Vec<GlobalAnnotation>, i64), sqlx::Error> {
    let ascending = matches!(direction, crate::listing::SortDirection::Asc);
    let rows = sqlx::query!(
        r#"
        SELECT
            g.id, g.arrangement_id, g.author_id,
            u.display_name as author_display_name,
            g.type as annotation_type, g.content,
            g.created_at as "created_at: DateTime<Utc>",
            g.updated_at as "updated_at: DateTime<Utc>",
            count(*) OVER() as "total!"
        FROM global_annotation g
        JOIN arrangement a ON a.id = g.arrangement_id AND a.deleted_at IS NULL
        JOIN "user" u ON u.id = g.author_id
        WHERE g.arrangement_id = $3
          AND ($4::text IS NULL OR g.type = $4)
        ORDER BY
            CASE WHEN $5 = 'createdAt' AND $6 THEN g.created_at END ASC,
            CASE WHEN $5 = 'createdAt' AND NOT $6 THEN g.created_at END DESC,
            CASE WHEN $5 = 'updatedAt' AND $6 THEN g.updated_at END ASC,
            CASE WHEN $5 = 'updatedAt' AND NOT $6 THEN g.updated_at END DESC,
            CASE WHEN $5 = 'type' AND $6 THEN g.type END ASC,
            CASE WHEN $5 = 'type' AND NOT $6 THEN g.type END DESC,
            g.created_at ASC,
            g.id ASC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
        arrangement_id,
        type_filter,
        sort.as_key(),
        ascending,
    )
    .fetch_all(pool)
    .await?;

    let total = match rows.first() {
        Some(row) => row.total,
        // An empty first page means there are none; only an empty page past
        // the start needs a separate count to report the real total.
        None if offset == 0 => 0,
        None => {
            sqlx::query_scalar!(
                r#"
                SELECT count(*) as "count!"
                FROM global_annotation g
                JOIN arrangement a ON a.id = g.arrangement_id AND a.deleted_at IS NULL
                WHERE g.arrangement_id = $1 AND ($2::text IS NULL OR g.type = $2)
                "#,
                arrangement_id,
                type_filter,
            )
            .fetch_one(pool)
            .await?
        }
    };
    let items = rows
        .into_iter()
        .map(|r| GlobalAnnotation {
            id: r.id,
            arrangement_id: r.arrangement_id,
            author_id: r.author_id,
            author_display_name: r.author_display_name,
            annotation_type: r.annotation_type,
            content: r.content,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Ok((items, total))
}

/// An annotation together with the title of the piece it belongs to — the
/// console's org-wide overview.
#[derive(Debug, Clone)]
pub struct OrgAnnotation {
    pub annotation: GlobalAnnotation,
    pub arrangement_title: String,
}

/// The most recently changed annotations across an org's live arrangements,
/// newest first, with the full count so a capped list can say so.
pub async fn list_for_org(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
) -> Result<(Vec<OrgAnnotation>, i64), sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            g.id, g.arrangement_id, g.author_id,
            u.display_name as author_display_name,
            g.type as annotation_type, g.content,
            g.created_at as "created_at: DateTime<Utc>",
            g.updated_at as "updated_at: DateTime<Utc>",
            a.title as arrangement_title,
            count(*) OVER() as "total!"
        FROM global_annotation g
        JOIN arrangement a ON a.id = g.arrangement_id AND a.deleted_at IS NULL
        JOIN "user" u ON u.id = g.author_id
        WHERE a.organization_id = $1
        ORDER BY g.updated_at DESC, g.id ASC
        LIMIT $2
        "#,
        organization_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let items = rows
        .into_iter()
        .map(|r| OrgAnnotation {
            annotation: GlobalAnnotation {
                id: r.id,
                arrangement_id: r.arrangement_id,
                author_id: r.author_id,
                author_display_name: r.author_display_name,
                annotation_type: r.annotation_type,
                content: r.content,
                created_at: r.created_at,
                updated_at: r.updated_at,
            },
            arrangement_title: r.arrangement_title,
        })
        .collect();
    Ok((items, total))
}

/// Change an annotation's type and/or content, if it is still at
/// `expected_version` (the ETag's millisecond `updated_at`). `None` leaves a
/// field as it is. Callers handle "nothing to change" themselves (returning
/// the current row), so this always writes.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    expected_version: i64,
    annotation_type: Option<&str>,
    content: Option<&str>,
) -> Result<GlobalAnnotation, WriteError> {
    validate(annotation_type, content)?;
    let updated = sqlx::query_as!(
        GlobalAnnotation,
        r#"
        WITH updated AS (
            UPDATE global_annotation g
            SET type = COALESCE($3, g.type),
                content = COALESCE($4, g.content),
                updated_at = now()
            FROM arrangement a
            WHERE g.id = $1
              AND a.id = g.arrangement_id
              AND a.deleted_at IS NULL
              AND floor(extract(epoch FROM g.updated_at) * 1000)::bigint = $2
            RETURNING g.*
        )
        SELECT
            g.id as "id!", g.arrangement_id as "arrangement_id!", g.author_id as "author_id!",
            u.display_name as "author_display_name!",
            g.type as "annotation_type!", g.content as "content!",
            g.created_at as "created_at!: DateTime<Utc>",
            g.updated_at as "updated_at!: DateTime<Utc>"
        FROM updated g
        JOIN "user" u ON u.id = g.author_id
        "#,
        id,
        expected_version,
        annotation_type,
        content,
    )
    .fetch_optional(pool)
    .await?;
    match updated {
        Some(row) => Ok(row),
        None if exists_live(pool, id).await? => Err(WriteError::Stale),
        None => Err(WriteError::NotFound),
    }
}

/// Remove an annotation for good, if it is still at `expected_version` and its
/// arrangement is live (a hidden annotation must survive to be restored).
/// Returns the row as it was, for the audit log.
pub async fn delete(
    pool: &PgPool,
    id: Uuid,
    expected_version: i64,
) -> Result<GlobalAnnotation, WriteError> {
    let deleted = sqlx::query_as!(
        GlobalAnnotation,
        r#"
        WITH deleted AS (
            DELETE FROM global_annotation g
            USING arrangement a
            WHERE g.id = $1
              AND a.id = g.arrangement_id
              AND a.deleted_at IS NULL
              AND floor(extract(epoch FROM g.updated_at) * 1000)::bigint = $2
            RETURNING g.*
        )
        SELECT
            g.id as "id!", g.arrangement_id as "arrangement_id!", g.author_id as "author_id!",
            u.display_name as "author_display_name!",
            g.type as "annotation_type!", g.content as "content!",
            g.created_at as "created_at!: DateTime<Utc>",
            g.updated_at as "updated_at!: DateTime<Utc>"
        FROM deleted g
        JOIN "user" u ON u.id = g.author_id
        "#,
        id,
        expected_version,
    )
    .fetch_optional(pool)
    .await?;
    match deleted {
        Some(row) => Ok(row),
        None if exists_live(pool, id).await? => Err(WriteError::Stale),
        None => Err(WriteError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vocabulary_matches_the_check_constraint() {
        for t in ["tempo", "dynamics", "articulation", "structure", "general"] {
            assert!(is_valid_type(t), "{t}");
        }
        assert!(!is_valid_type("Tempo"));
        assert!(!is_valid_type(""));
        assert!(!is_valid_type("cut"));
    }

    #[test]
    fn the_invalid_type_message_lists_the_whole_vocabulary() {
        let message = invalid_type_message();
        for t in TYPES {
            assert!(message.contains(t), "{message} is missing {t}");
        }
    }

    #[test]
    fn every_allowlisted_sort_key_names_a_sort_field() {
        for (_, key) in SORT_ALLOWLIST {
            assert!(SortField::from_key(key).is_some(), "{key}");
        }
    }

    #[test]
    fn blank_content_and_unknown_types_are_rejected() {
        assert!(validate(Some("tempo"), Some("bar 12: poco rit.")).is_ok());
        assert!(
            validate(None, None).is_ok(),
            "a partial update may change nothing"
        );
        assert!(matches!(
            validate(Some("tempo"), Some("   ")),
            Err(GlobalAnnotationError::EmptyContent)
        ));
        assert!(matches!(
            validate(Some("cut"), Some("x")),
            Err(GlobalAnnotationError::InvalidType)
        ));
    }
}
