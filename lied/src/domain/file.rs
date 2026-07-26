//! File: a stored representation of a voice or full score in a specific
//! format (CLAUDE.md Entities: File). Issue #7 adds upload/download with the
//! content streamed to/from MinIO; this module owns the DB rows and the
//! derived-storage-key computation. The actual byte streaming lives in
//! [`crate::storage`].
//!
//! **Derived storage key.** The MinIO object key is NOT stored — it is
//! computed from the entity tree (CLAUDE.md Decisions: storage path), so the
//! REST view and the WebDAV view always agree:
//!   - voice file:      `orgs/<org>/arrangements/<arr>/voices/<voice>/<name>.<ext>`
//!   - full-score file: `orgs/<org>/arrangements/<arr>/score/<name>.<ext>`
//!
//! **Atomic replacement.** Replacing a file is never an in-place mutation:
//! [`replace`] inserts a new row and soft-deletes the old one in one
//! transaction, preserving the `derived_from_file_id` chain and audit history
//! (CLAUDE.md Decisions: atomic file replacement).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Wire representation of a `file` row. `camelCase` per CLAUDE.md's JSON
/// convention. The MinIO key is derived, not a column — see [`derived_key`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub id: Uuid,
    pub arrangement_id: Uuid,
    /// `None` for a full-score file.
    pub voice_id: Option<Uuid>,
    /// Filename without extension.
    pub name: String,
    /// Coarse routing key: `lilypond` | `musicxml` | `pdf` | `image`.
    pub format: String,
    pub mime_type: String,
    pub derived_from_file_id: Option<Uuid>,
    pub conversion_quality: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(thiserror::Error, Debug)]
pub enum FileError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("a file with this name and format already exists here")]
    Duplicate,
    #[error("unsupported or unrecognized mime type")]
    UnsupportedMime,
    /// The file targeted by a replace was concurrently soft-deleted between
    /// the caller's lookup and the replace transaction.
    #[error("the file being replaced no longer exists")]
    ReplaceTargetGone,
}

/// Map a MIME type to the coarse `format` routing key and the canonical file
/// extension used in the derived MinIO key (CLAUDE.md: `format` is derived
/// from `mime_type` via a lookup table; the invariant is they must agree).
/// Returns `None` for an unrecognized type so the caller can 415/400.
pub fn format_and_ext_for_mime(mime: &str) -> Option<(&'static str, &'static str)> {
    // Normalize: drop any `; charset=…` parameter and lowercase.
    let base = mime.split(';').next().unwrap_or(mime).trim();
    let base = base.to_ascii_lowercase();
    let pair = match base.as_str() {
        "application/x-lilypond" | "text/x-lilypond" => ("lilypond", "ly"),
        "application/vnd.recordare.musicxml+xml" | "application/vnd.recordare.musicxml" => {
            ("musicxml", "musicxml")
        }
        "application/pdf" => ("pdf", "pdf"),
        "image/png" => ("image", "png"),
        "image/jpeg" => ("image", "jpg"),
        "image/tiff" => ("image", "tif"),
        "image/gif" => ("image", "gif"),
        "image/webp" => ("image", "webp"),
        "image/bmp" => ("image", "bmp"),
        _ => return None,
    };
    Some(pair)
}

/// Map a lowercase file extension to its canonical MIME type — the reverse of
/// [`format_and_ext_for_mime`], used on WebDAV `PUT` where the client provides
/// a filename (with extension) but rarely a reliable `Content-Type`. Returns
/// `None` for an unsupported extension (caller maps to a WebDAV `403`).
pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    let e = ext.to_ascii_lowercase();
    let mime = match e.as_str() {
        "ly" => "application/x-lilypond",
        "musicxml" => "application/vnd.recordare.musicxml+xml",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "tif" | "tiff" => "image/tiff",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    };
    Some(mime)
}

/// Split a WebDAV filename into `(stem, extension)` on the last `.`. Returns
/// `None` if there is no extension (a File row always has a format/extension).
pub fn split_filename(filename: &str) -> Option<(&str, &str)> {
    let (stem, ext) = filename.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    Some((stem, ext))
}

/// Build the WebDAV filename (`<name>.<ext>`) for a stored file, deriving the
/// extension from its `mime_type`. Returns `None` if the mime is unrecognized
/// (which would be a stored-data bug, since it passed the same table on write).
pub fn webdav_filename(name: &str, mime_type: &str) -> Option<String> {
    let (_format, ext) = format_and_ext_for_mime(mime_type)?;
    Some(format!("{name}.{ext}"))
}

/// Compute the derived MinIO object key from the entity-tree slugs. `voice`
/// is `Some` for a voice file, `None` for a full-score file.
pub fn derived_key(
    org_slug: &str,
    arrangement_slug: &str,
    voice_slug: Option<&str>,
    name: &str,
    ext: &str,
) -> String {
    match voice_slug {
        Some(voice) => {
            format!("orgs/{org_slug}/arrangements/{arrangement_slug}/voices/{voice}/{name}.{ext}")
        }
        None => {
            format!("orgs/{org_slug}/arrangements/{arrangement_slug}/score/{name}.{ext}")
        }
    }
}

/// The slugs needed to build a derived key for a file under
/// `(arrangement_id, voice_id)`. `voice_slug` is `None` for full-score files.
pub struct PathSlugs {
    pub org_slug: String,
    pub arrangement_slug: String,
    pub voice_slug: Option<String>,
}

/// Resolve the org / arrangement / (optional) voice slugs for a file's
/// location. Returns `None` if the arrangement (or the named voice) does not
/// exist as a live row. Validates that `voice_id`, when given, belongs to
/// `arrangement_id`.
pub async fn resolve_path_slugs(
    pool: &PgPool,
    arrangement_id: Uuid,
    voice_id: Option<Uuid>,
) -> Result<Option<PathSlugs>, sqlx::Error> {
    // Always need the org + arrangement slug.
    let arr = sqlx::query!(
        r#"
        SELECT o.slug AS org_slug, a.slug AS arrangement_slug
        FROM arrangement a
        JOIN organization o ON o.id = a.organization_id
        WHERE a.id = $1 AND a.deleted_at IS NULL
        "#,
        arrangement_id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(arr) = arr else {
        return Ok(None);
    };

    let voice_slug = match voice_id {
        Some(vid) => {
            let v = sqlx::query!(
                r#"SELECT slug FROM voice WHERE id = $1 AND arrangement_id = $2 AND deleted_at IS NULL"#,
                vid,
                arrangement_id,
            )
            .fetch_optional(pool)
            .await?;
            match v {
                Some(v) => Some(v.slug),
                // Named voice doesn't belong to this arrangement (or is gone).
                None => return Ok(None),
            }
        }
        None => None,
    };

    Ok(Some(PathSlugs {
        org_slug: arr.org_slug,
        arrangement_slug: arr.arrangement_slug,
        voice_slug,
    }))
}

/// Fields for inserting a new file row. The MinIO object is uploaded
/// separately (see [`crate::storage`]); this only records the DB identity.
pub struct NewFile<'a> {
    pub arrangement_id: Uuid,
    pub voice_id: Option<Uuid>,
    pub name: &'a str,
    pub format: &'a str,
    pub mime_type: &'a str,
    pub derived_from_file_id: Option<Uuid>,
    pub conversion_quality: Option<&'a str>,
    pub created_by: Option<Uuid>,
}

fn map_write_error(err: sqlx::Error) -> FileError {
    if let sqlx::Error::Database(ref db_err) = err {
        if let Some(c) = db_err.constraint() {
            if c == "file_voice_file_key" || c == "file_score_file_key" {
                return FileError::Duplicate;
            }
        }
    }
    FileError::Database(err)
}

/// Insert a file row. Maps the unique-index violation (live duplicate
/// `(arrangement, voice, name, format)`) to [`FileError::Duplicate`].
pub async fn create(pool: &PgPool, id: Uuid, new: NewFile<'_>) -> Result<File, FileError> {
    let row = sqlx::query_as!(
        File,
        r#"
        INSERT INTO file (
            id, arrangement_id, voice_id, name, format, mime_type,
            derived_from_file_id, conversion_quality, created_by
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING
            id, arrangement_id, voice_id, name, format, mime_type,
            derived_from_file_id, conversion_quality,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        id,
        new.arrangement_id,
        new.voice_id,
        new.name,
        new.format,
        new.mime_type,
        new.derived_from_file_id,
        new.conversion_quality,
        new.created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_write_error)?;

    Ok(row)
}

/// Find a live file by its logical location `(arrangement, voice, name,
/// format)` — the natural key WebDAV addresses a file by (score files have
/// `voice_id = None`). Returns `None` if absent or soft-deleted.
pub async fn find_by_location(
    pool: &PgPool,
    arrangement_id: Uuid,
    voice_id: Option<Uuid>,
    name: &str,
    format: &str,
) -> Result<Option<File>, sqlx::Error> {
    sqlx::query_as!(
        File,
        r#"
        SELECT
            id, arrangement_id, voice_id, name, format, mime_type,
            derived_from_file_id, conversion_quality,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM file
        WHERE arrangement_id = $1
          AND ($2::uuid IS NULL AND voice_id IS NULL OR voice_id = $2)
          AND name = $3 AND format = $4
          AND deleted_at IS NULL
        "#,
        arrangement_id,
        voice_id,
        name,
        format,
    )
    .fetch_optional(pool)
    .await
}

/// Look up a live file by id.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<File>, sqlx::Error> {
    sqlx::query_as!(
        File,
        r#"
        SELECT
            id, arrangement_id, voice_id, name, format, mime_type,
            derived_from_file_id, conversion_quality,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM file
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Look up a file by id including soft-deleted rows (audit / undelete flows).
pub async fn find_by_id_including_deleted(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<File>, sqlx::Error> {
    sqlx::query_as!(
        File,
        r#"
        SELECT
            id, arrangement_id, voice_id, name, format, mime_type,
            derived_from_file_id, conversion_quality,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        FROM file
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// List live files under an arrangement, optionally scoped to a voice.
/// `voice_id = Some(v)` lists that voice's files; `voice_id = None` lists the
/// full-score files (`voice_id IS NULL`). Ordered newest-first. Applies the
/// hide-with-references filter on the parent arrangement.
pub async fn list(
    pool: &PgPool,
    arrangement_id: Uuid,
    voice_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<File>, i64), sqlx::Error> {
    let rows = sqlx::query_as!(
        File,
        r#"
        SELECT
            f.id, f.arrangement_id, f.voice_id, f.name, f.format, f.mime_type,
            f.derived_from_file_id, f.conversion_quality,
            f.created_at as "created_at: DateTime<Utc>",
            f.updated_at as "updated_at: DateTime<Utc>",
            f.created_by,
            f.deleted_at as "deleted_at: DateTime<Utc>"
        FROM file f
        JOIN arrangement a ON a.id = f.arrangement_id
        WHERE f.arrangement_id = $1
          AND f.deleted_at IS NULL
          AND a.deleted_at IS NULL
          AND ($2::uuid IS NULL AND f.voice_id IS NULL OR f.voice_id = $2)
        ORDER BY f.created_at DESC, f.id ASC
        LIMIT $3 OFFSET $4
        "#,
        arrangement_id,
        voice_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) as "count!"
        FROM file f
        JOIN arrangement a ON a.id = f.arrangement_id
        WHERE f.arrangement_id = $1
          AND f.deleted_at IS NULL
          AND a.deleted_at IS NULL
          AND ($2::uuid IS NULL AND f.voice_id IS NULL OR f.voice_id = $2)
        "#,
        arrangement_id,
        voice_id,
    )
    .fetch_one(pool)
    .await?;

    Ok((rows, total))
}

/// The soft-deleted files for an arrangement (or one of its voices when
/// `voice_id` is `Some`), most-recently-deleted first. Powers the console's
/// "previous / deleted versions" list — a replaced file leaves its old row
/// soft-deleted here, and each is restorable. The parent arrangement must be
/// live (a soft-deleted arrangement hides its whole subtree).
pub async fn list_deleted(
    pool: &PgPool,
    arrangement_id: Uuid,
    voice_id: Option<Uuid>,
) -> Result<Vec<File>, sqlx::Error> {
    let rows = sqlx::query_as!(
        File,
        r#"
        SELECT
            f.id, f.arrangement_id, f.voice_id, f.name, f.format, f.mime_type,
            f.derived_from_file_id, f.conversion_quality,
            f.created_at as "created_at: DateTime<Utc>",
            f.updated_at as "updated_at: DateTime<Utc>",
            f.created_by,
            f.deleted_at as "deleted_at: DateTime<Utc>"
        FROM file f
        JOIN arrangement a ON a.id = f.arrangement_id
        WHERE f.arrangement_id = $1
          AND f.deleted_at IS NOT NULL
          AND a.deleted_at IS NULL
          AND ($2::uuid IS NULL AND f.voice_id IS NULL OR f.voice_id = $2)
        ORDER BY f.deleted_at DESC, f.id ASC
        "#,
        arrangement_id,
        voice_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Atomically replace a file: soft-delete `old_id` and insert a new row that
/// records `derived_from_file_id = old_id`-chain continuity, in ONE
/// transaction (CLAUDE.md: never in-place). The new row's
/// `derived_from_file_id` is carried from `new.derived_from_file_id` (callers
/// pass the source-of-derivation, not the replaced row — replacement is an
/// identity swap, not a derivation). Returns the new row.
pub async fn replace(
    pool: &PgPool,
    old_id: Uuid,
    new_id: Uuid,
    new: NewFile<'_>,
) -> Result<File, FileError> {
    let mut tx = pool.begin().await?;

    let deleted = sqlx::query!(
        r#"UPDATE file SET deleted_at = now(), updated_at = now()
           WHERE id = $1 AND deleted_at IS NULL"#,
        old_id,
    )
    .execute(&mut *tx)
    .await?;

    if deleted.rows_affected() == 0 {
        // Old row vanished concurrently; abort so we don't leave two live rows.
        // Surfaced as a 404, not a 500 (the target is simply gone).
        tx.rollback().await?;
        return Err(FileError::ReplaceTargetGone);
    }

    let row = sqlx::query_as!(
        File,
        r#"
        INSERT INTO file (
            id, arrangement_id, voice_id, name, format, mime_type,
            derived_from_file_id, conversion_quality, created_by
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING
            id, arrangement_id, voice_id, name, format, mime_type,
            derived_from_file_id, conversion_quality,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>",
            created_by,
            deleted_at as "deleted_at: DateTime<Utc>"
        "#,
        new_id,
        new.arrangement_id,
        new.voice_id,
        new.name,
        new.format,
        new.mime_type,
        new.derived_from_file_id,
        new.conversion_quality,
        new.created_by,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(map_write_error)?;

    tx.commit().await?;
    Ok(row)
}

/// Soft-delete a file (DB row hidden; MinIO object retained per CLAUDE.md:
/// "DB is source of truth for existence; MinIO versioning is content
/// history"). Returns `true` if a live row was hidden.
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let res = sqlx::query!(
        r#"UPDATE file SET deleted_at = now(), updated_at = now()
           WHERE id = $1 AND deleted_at IS NULL"#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Restore a soft-deleted file. If a *live* file already occupies the same
/// `(arrangement, voice, name, format)` slot — e.g. this row was replaced by a
/// newer upload — the partial unique index rejects the restore, surfaced as
/// [`FileError::Duplicate`] (the caller shows a "a current file already holds
/// that name/format" message). Returns `false` if no soft-deleted row matched.
pub async fn undelete(pool: &PgPool, id: Uuid) -> Result<bool, FileError> {
    let res = sqlx::query!(
        r#"UPDATE file SET deleted_at = NULL, updated_at = now()
           WHERE id = $1 AND deleted_at IS NOT NULL"#,
        id,
    )
    .execute(pool)
    .await
    .map_err(map_write_error)?;
    Ok(res.rows_affected() > 0)
}

/// Compensating action for a [`replace`] whose DB swap committed but whose
/// subsequent object upload failed: drop the just-inserted `new_id` row and
/// restore `old_id` as the live file, returning to the pre-replace state.
/// Soft-deletes the new row *before* undeleting the old one, so the partial
/// unique index never momentarily sees two live rows for the same key.
pub async fn restore_replaced(
    pool: &PgPool,
    old_id: Uuid,
    new_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"UPDATE file SET deleted_at = now(), updated_at = now() WHERE id = $1"#,
        new_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"UPDATE file SET deleted_at = NULL, updated_at = now() WHERE id = $1"#,
        old_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_lookup_covers_the_four_formats() {
        assert_eq!(
            format_and_ext_for_mime("application/pdf"),
            Some(("pdf", "pdf"))
        );
        assert_eq!(
            format_and_ext_for_mime("application/vnd.recordare.musicxml+xml"),
            Some(("musicxml", "musicxml"))
        );
        assert_eq!(
            format_and_ext_for_mime("application/x-lilypond"),
            Some(("lilypond", "ly"))
        );
        assert_eq!(format_and_ext_for_mime("image/png"), Some(("image", "png")));
        assert_eq!(
            format_and_ext_for_mime("image/jpeg"),
            Some(("image", "jpg"))
        );
        // charset parameter tolerated, case-insensitive.
        assert_eq!(
            format_and_ext_for_mime("APPLICATION/PDF; charset=binary"),
            Some(("pdf", "pdf"))
        );
        assert_eq!(format_and_ext_for_mime("application/zip"), None);
    }

    #[test]
    fn derived_key_distinguishes_voice_and_score() {
        assert_eq!(
            derived_key("acme", "bolero", Some("flute-1"), "part", "pdf"),
            "orgs/acme/arrangements/bolero/voices/flute-1/part.pdf"
        );
        assert_eq!(
            derived_key("acme", "bolero", None, "full-score", "pdf"),
            "orgs/acme/arrangements/bolero/score/full-score.pdf"
        );
    }
}
