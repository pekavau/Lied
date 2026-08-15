//! Shared file upload/replace orchestration, called by **both** the `/v1` REST
//! tree ([`crate::routes::files`]) and the `/admin` console
//! ([`crate::routes::admin::files`]). Keeping the create-then-stream, key
//! derivation, provenance, format rules, and rollback in one place stops the
//! two surfaces (and a future MCP tool) from diverging — CLAUDE.md: business
//! logic lives in the domain/service layer, not duplicated in HTTP handlers.
//!
//! Each caller maps [`UploadError`] to its own response style (RFC 7807 for
//! `/v1`, HTML for the console).

use uuid::Uuid;

use crate::domain::file;
use crate::state::AppState;
use crate::storage::{self, ChunkSource};

/// A stored file: the row, the byte count that reached object storage, and the
/// derived object key. The key is returned (not just used internally) so every
/// caller can record it in the audit payload — when the DB and MinIO disagree,
/// "which object did this write touch" is the field that resolves it.
#[derive(Debug)]
pub struct Stored {
    pub file: file::File,
    pub bytes: u64,
    pub key: String,
}

/// Why a store operation failed, independent of the HTTP surface.
#[derive(thiserror::Error, Debug)]
pub enum UploadError {
    #[error("mime type '{0}' is not a stored format")]
    UnsupportedMime(String),
    /// The *stored* file's `mime_type` is not in the lookup table — a
    /// corrupt-row bug on our side, not a bad request. Kept distinct from
    /// [`UploadError::UnsupportedMime`] so it surfaces as a 500 rather than
    /// blaming the client for a media type they never sent.
    #[error("stored file has an unrecognized mime type '{0}'")]
    StoredMimeUnknown(String),
    #[error("the file needs a name")]
    EmptyName,
    #[error("a replacement must keep the original's format ({expected})")]
    FormatMismatch { expected: String },
    #[error("a file with this name and format already exists here")]
    Duplicate,
    #[error("the arrangement or voice no longer exists")]
    NotFound,
    #[error("the file being replaced no longer exists")]
    ReplaceTargetGone,
    #[error("upload exceeds the maximum allowed size of {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("database error")]
    Db(#[from] sqlx::Error),
    #[error("storage error: {0}")]
    Storage(String),
}

fn map_file_error(err: file::FileError) -> UploadError {
    match err {
        file::FileError::Duplicate => UploadError::Duplicate,
        file::FileError::ReplaceTargetGone | file::FileError::Superseded => {
            UploadError::ReplaceTargetGone
        }
        file::FileError::UnsupportedMime => UploadError::StoredMimeUnknown("unknown".to_string()),
        file::FileError::Database(e) => UploadError::Db(e),
    }
}

fn map_storage_error(err: storage::StorageError) -> UploadError {
    match err {
        storage::StorageError::TooLarge { limit } => UploadError::TooLarge { limit },
        other => UploadError::Storage(other.to_string()),
    }
}

/// Store an uploaded file: validate the mime/name, insert the row (cheap
/// duplicate detection), then stream the bytes to MinIO. On a streaming
/// failure the never-written row is **hard-deleted** so no phantom remains.
pub async fn store_upload(
    state: &AppState,
    arrangement_id: Uuid,
    voice_id: Option<Uuid>,
    file_name: &str,
    mime: &str,
    created_by: Option<Uuid>,
    chunk: impl ChunkSource,
) -> Result<Stored, UploadError> {
    let (format, ext) = file::format_and_ext_for_mime(mime)
        .ok_or_else(|| UploadError::UnsupportedMime(mime.to_string()))?;
    let name = file::name_stem(file_name).ok_or(UploadError::EmptyName)?;
    let slugs = file::resolve_path_slugs(&state.db, arrangement_id, voice_id)
        .await?
        .ok_or(UploadError::NotFound)?;
    let key = file::derived_key(
        &slugs.org_slug,
        &slugs.arrangement_slug,
        slugs.voice_slug.as_deref(),
        &name,
        ext,
    );

    let id = Uuid::now_v7();
    let created = file::create(
        &state.db,
        id,
        file::NewFile {
            arrangement_id,
            voice_id,
            name: &name,
            format,
            mime_type: mime,
            derived_from_file_id: None,
            conversion_quality: None,
            created_by,
        },
    )
    .await
    .map_err(map_file_error)?;

    match storage::upload_streaming(
        &state.s3,
        &state.config.s3_bucket,
        &key,
        mime,
        state.config.max_upload_bytes,
        chunk,
    )
    .await
    {
        Ok(bytes) => Ok(Stored {
            file: created,
            bytes,
            key,
        }),
        Err(error) => {
            // The row references an object that was never written — remove it
            // entirely (not soft-delete) so it can't be listed/restored. If the
            // compensating delete ALSO fails we have a phantom row pointing at
            // nothing, which is exactly the state an operator must be able to
            // find later — never swallow it.
            if let Err(rollback_error) = file::hard_delete(&state.db, id).await {
                tracing::error!(
                    %rollback_error,
                    file_id = %id,
                    key = %key,
                    "failed to roll back the row for an upload that never stored its bytes"
                );
            }
            Err(map_storage_error(error))
        }
    }
}

/// Replace an existing file with new bytes. A replacement is an **identity
/// swap**: it must keep the original's format *and* extension (so the derived
/// key and the stored object are the same — the object is overwritten, never
/// orphaned), and it carries the original's mime, provenance
/// (`derived_from_file_id`), and `conversion_quality` onto the new row. On a
/// streaming failure the row swap is undone, leaving the original live.
pub async fn store_replacement(
    state: &AppState,
    old: &file::File,
    mime: &str,
    created_by: Option<Uuid>,
    chunk: impl ChunkSource,
) -> Result<Stored, UploadError> {
    // An unrecognized mime on the STORED row is our data bug (it passed this
    // same table on write), not a client error — see `StoredMimeUnknown`.
    let (old_format, old_ext) = file::format_and_ext_for_mime(&old.mime_type)
        .ok_or_else(|| UploadError::StoredMimeUnknown(old.mime_type.clone()))?;
    let new_pair = file::format_and_ext_for_mime(mime)
        .ok_or_else(|| UploadError::UnsupportedMime(mime.to_string()))?;
    if new_pair != (old_format, old_ext) {
        return Err(UploadError::FormatMismatch {
            expected: format!("{old_format} (.{old_ext})"),
        });
    }

    let slugs = file::resolve_path_slugs(&state.db, old.arrangement_id, old.voice_id)
        .await?
        .ok_or(UploadError::NotFound)?;
    // Old name + old ext → the SAME key the original occupies.
    let key = file::derived_key(
        &slugs.org_slug,
        &slugs.arrangement_slug,
        slugs.voice_slug.as_deref(),
        &old.name,
        old_ext,
    );

    let new_id = Uuid::now_v7();
    let created = file::replace(
        &state.db,
        old.id,
        new_id,
        file::NewFile {
            arrangement_id: old.arrangement_id,
            voice_id: old.voice_id,
            name: &old.name,
            format: old_format,
            mime_type: &old.mime_type,
            derived_from_file_id: old.derived_from_file_id,
            conversion_quality: old.conversion_quality.as_deref(),
            created_by,
        },
    )
    .await
    .map_err(map_file_error)?;

    match storage::upload_streaming(
        &state.s3,
        &state.config.s3_bucket,
        &key,
        &old.mime_type,
        state.config.max_upload_bytes,
        chunk,
    )
    .await
    {
        Ok(bytes) => Ok(Stored {
            file: created,
            bytes,
            key,
        }),
        Err(error) => {
            // Undo the row swap: drop the new row, re-live the old one. A
            // failure here leaves the file soft-deleted with its bytes intact
            // and no live row — recoverable, but only if it's visible.
            if let Err(rollback_error) = file::restore_replaced(&state.db, old.id, new_id).await {
                tracing::error!(
                    %rollback_error,
                    old_file_id = %old.id,
                    new_file_id = %new_id,
                    key = %key,
                    "failed to undo a replace whose bytes never stored; the old file is left soft-deleted"
                );
            }
            Err(map_storage_error(error))
        }
    }
}
