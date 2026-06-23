//! Instrument: instance-wide controlled vocabulary (CLAUDE.md Entities:
//! Instrument). Read-only from the application's perspective in phase 1 —
//! only `GET /v1/instruments` exists; admin add/edit lands later behind
//! `is_system_admin`.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of an `instrument` row. `camelCase` per CLAUDE.md's
/// JSON-casing convention.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Instrument {
    pub id: Uuid,
    pub key: String,
    pub display_name: String,
    pub aliases: Vec<String>,
    pub family: String,
    pub transposition: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fetch one page of instruments plus the total live row count, for the
/// `{ items, total, limit, offset }` envelope.
///
/// Ordered by `(display_name, id)` — `display_name` alone is not unique, so a
/// stable tiebreaker is required, otherwise offset pagination could skip or
/// duplicate rows across pages when two instruments share a display name.
///
/// `count(*) OVER()` returns the full live total alongside each row, so the
/// common case is a single round-trip. The window count is only emitted on
/// rows that are returned, so an offset past the end yields an empty page
/// with no total; there we fall back to a dedicated `count(*)` so `total`
/// stays accurate rather than collapsing to 0.
pub async fn list(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Instrument>, i64), sqlx::Error> {
    // `created_at`/`updated_at` get an explicit `chrono::DateTime<Utc>`
    // override: sqlx has both its `chrono` and `time` Cargo features enabled
    // in this workspace (the latter pulled in transitively by
    // `tower-sessions-sqlx-store`, which requires sqlx's `time` feature for
    // its own session-expiry column), and when both are present the
    // `query!` macro defaults `timestamptz` columns to
    // `time::OffsetDateTime`. The `as "col: Type"` cast syntax pins the
    // mapping to `chrono` explicitly, per CLAUDE.md's "Datetime crate:
    // chrono" decision. The `total!` cast asserts the window count is
    // non-null (it always is).
    let rows = sqlx::query!(
        r#"
        SELECT
            id,
            key,
            display_name,
            aliases,
            family,
            transposition,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            count(*) OVER() as "total!"
        FROM instrument
        ORDER BY display_name ASC, id ASC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset
    )
    .fetch_all(pool)
    .await?;

    let total = match rows.first() {
        Some(row) => row.total,
        // Empty page (offset past the end): the window count returned no
        // rows, so ask for the total directly.
        None => sqlx::query_scalar!(r#"SELECT count(*) FROM instrument"#)
            .fetch_one(pool)
            .await?
            .unwrap_or(0),
    };

    let items = rows
        .into_iter()
        .map(|row| Instrument {
            id: row.id,
            key: row.key,
            display_name: row.display_name,
            aliases: row.aliases,
            family: row.family,
            transposition: row.transposition,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
        .collect();

    Ok((items, total))
}
