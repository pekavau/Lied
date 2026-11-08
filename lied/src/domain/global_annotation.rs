//! GlobalAnnotation: a conductor's note on an arrangement that applies to the
//! whole ensemble — a tempo decision, a cut, a dynamic shape (CLAUDE.md
//! Entities: GlobalAnnotation; UC-16). Issue #36 activates the authoring half;
//! musicians read them in the phase-3 client, not over WebDAV.
//!
//! **Hard delete.** `global_annotation` has no `deleted_at` (CLAUDE.md
//! Unique-constraints table), so [`delete`] removes the row. The route records
//! the deleted content in the audit log, which is then the only copy.
//!
//! **Scoped through the arrangement.** An annotation has no `organization_id`;
//! its org is its arrangement's. Every read here joins the arrangement and
//! requires it to be live, so a soft-deleted arrangement hides its annotations
//! (hide-with-references) without touching them, and restoring the arrangement
//! brings them back.
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

/// Wire representation of an annotation, with its author's display name so a
/// list can be rendered without a lookup per row.
#[derive(Debug, Clone, Serialize, ToSchema)]
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
    #[error("type must be one of tempo, dynamics, articulation, structure, general")]
    InvalidType,
    #[error("content must not be empty")]
    EmptyContent,
    /// The arrangement is gone (or was soft-deleted) by the time of the write.
    #[error("the arrangement does not exist")]
    ArrangementGone,
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

/// Add an annotation to a live arrangement. Atomic with the liveness check:
/// the INSERT selects from the live arrangement row, so an arrangement removed
/// in the meantime yields [`GlobalAnnotationError::ArrangementGone`] and no row.
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    arrangement_id: Uuid,
    author_id: Uuid,
    annotation_type: &str,
    content: &str,
) -> Result<GlobalAnnotation, GlobalAnnotationError> {
    validate(Some(annotation_type), Some(content))?;
    let inserted = sqlx::query_scalar!(
        r#"
        INSERT INTO global_annotation (id, arrangement_id, author_id, type, content, created_by)
        SELECT $1, a.id, $3, $4, $5, $3
        FROM arrangement a
        WHERE a.id = $2 AND a.deleted_at IS NULL
        RETURNING id
        "#,
        id,
        arrangement_id,
        author_id,
        annotation_type,
        content,
    )
    .fetch_optional(pool)
    .await?;
    if inserted.is_none() {
        return Err(GlobalAnnotationError::ArrangementGone);
    }
    // Re-read through the one SELECT shape so the author name is filled in.
    load(pool, id)
        .await?
        .ok_or(GlobalAnnotationError::ArrangementGone)
}

/// One annotation by id, regardless of org — internal; callers outside this
/// module use the org-scoped [`find`].
async fn load(pool: &PgPool, id: Uuid) -> Result<Option<GlobalAnnotation>, sqlx::Error> {
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
        WHERE g.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
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

/// Sort allowlist for the `/v1` list. Default `createdAt:asc` — the order the
/// conductor wrote them, which is usually rehearsal order.
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[
    ("createdAt", "g.created_at"),
    ("updatedAt", "g.updated_at"),
    ("type", "g.type"),
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
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    type_filter: Option<&str>,
) -> Result<(Vec<GlobalAnnotation>, i64), sqlx::Error> {
    // `sort_column` comes only from SORT_ALLOWLIST (resolved by the caller via
    // `listing::resolve_sort`) and the direction from a closed enum, so the
    // formatted fragment cannot carry user text — the same pattern as
    // `voice::list_for_arrangement`.
    let direction = sort_direction.as_sql();
    let query = format!(
        r#"
        SELECT
            g.id, g.arrangement_id, g.author_id,
            u.display_name as author_display_name,
            g.type as annotation_type, g.content,
            g.created_at, g.updated_at,
            count(*) OVER() as total
        FROM global_annotation g
        JOIN arrangement a ON a.id = g.arrangement_id AND a.deleted_at IS NULL
        JOIN "user" u ON u.id = g.author_id
        WHERE g.arrangement_id = $3
          AND ($4::text IS NULL OR g.type = $4)
        ORDER BY {sort_column} {direction}, g.id ASC
        LIMIT $1 OFFSET $2
        "#
    );
    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(arrangement_id)
        .bind(type_filter)
        .fetch_all(pool)
        .await?;

    use sqlx::Row as _;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
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
        .map(|row| {
            Ok(GlobalAnnotation {
                id: row.try_get("id")?,
                arrangement_id: row.try_get("arrangement_id")?,
                author_id: row.try_get("author_id")?,
                author_display_name: row.try_get("author_display_name")?,
                annotation_type: row.try_get("annotation_type")?,
                content: row.try_get("content")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
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
            a.title as arrangement_title
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
    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) as "count!"
        FROM global_annotation g
        JOIN arrangement a ON a.id = g.arrangement_id AND a.deleted_at IS NULL
        WHERE a.organization_id = $1
        "#,
        organization_id,
    )
    .fetch_one(pool)
    .await?;
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

/// Change an annotation's type and/or content; `None` leaves a field as it is
/// (a real partial update). Returns `None` if no live annotation matched —
/// including one whose arrangement was soft-deleted since it was read.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    annotation_type: Option<&str>,
    content: Option<&str>,
) -> Result<Option<GlobalAnnotation>, GlobalAnnotationError> {
    validate(annotation_type, content)?;
    let updated = sqlx::query_scalar!(
        r#"
        UPDATE global_annotation g
        SET type = COALESCE($2, g.type),
            content = COALESCE($3, g.content),
            updated_at = now()
        FROM arrangement a
        WHERE g.id = $1 AND a.id = g.arrangement_id AND a.deleted_at IS NULL
        RETURNING g.id
        "#,
        id,
        annotation_type,
        content,
    )
    .fetch_optional(pool)
    .await?;
    match updated {
        Some(_) => Ok(load(pool, id).await?),
        None => Ok(None),
    }
}

/// Remove an annotation for good. Returns `false` if no row matched.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(r#"DELETE FROM global_annotation WHERE id = $1"#, id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
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
