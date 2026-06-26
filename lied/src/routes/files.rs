//! File upload / download for arrangements and voices (issue #7).
//!
//! Streamed end-to-end to/from MinIO ([`crate::storage`]); the MinIO object
//! key is derived from the entity tree ([`crate::domain::file`]). Routes
//! mirror the nesting of the entities:
//!   - full-score files: `/v1/orgs/{org}/arrangements/{arr}/files[/{file}]`
//!   - voice files:      `/v1/orgs/{org}/arrangements/{arr}/voices/{voice}/files[/{file}]`
//!
//! Both POST (create) and PUT (replace) are `owner`/`archivist`-gated and
//! audited; GET (download/list) requires at least `musician`. The upload
//! routes disable axum's default body limit and instead enforce
//! `LIED_MAX_UPLOAD_BYTES` while streaming (→ 413).

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::authz::require_org_role_v1;
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::Role;
use crate::domain::{arrangement, file, voice};
use crate::error::AppError;
use crate::pagination::{Page, PageParams};
use crate::routes::RequestId;
use crate::state::AppState;
use crate::storage::{self, StorageError};

pub fn router() -> Router<AppState> {
    Router::new()
        // Full-score files.
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files",
            get(list_score_files).post(upload_score_file),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files/:file_id",
            get(download_score_file)
                .put(replace_score_file)
                .delete(delete_score_file),
        )
        // Voice files.
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files",
            get(list_voice_files).post(upload_voice_file),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files/:file_id",
            get(download_voice_file)
                .put(replace_voice_file)
                .delete(delete_voice_file),
        )
        // Uploads stream to MinIO with a manual byte ceiling, so the default
        // 2 MiB extractor limit must be lifted on these routes only.
        .layer(DefaultBodyLimit::disable())
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct FileResponse {
    id: Uuid,
    arrangement_id: Uuid,
    voice_id: Option<Uuid>,
    name: String,
    format: String,
    mime_type: String,
    derived_from_file_id: Option<Uuid>,
    conversion_quality: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<file::File> for FileResponse {
    fn from(f: file::File) -> Self {
        Self {
            id: f.id,
            arrangement_id: f.arrangement_id,
            voice_id: f.voice_id,
            name: f.name,
            format: f.format,
            mime_type: f.mime_type,
            derived_from_file_id: f.derived_from_file_id,
            conversion_quality: f.conversion_quality,
            created_at: f.created_at,
            updated_at: f.updated_at,
            deleted_at: f.deleted_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Scoping helpers — verify org ownership through the arrangement (and, for
// voice files, that the voice belongs to the arrangement). Mirrors the
// cross-org lesson from issue #6: a file has no org of its own.
// ---------------------------------------------------------------------------

async fn scope_arrangement(
    state: &AppState,
    org_id: Uuid,
    arr_id: Uuid,
) -> Result<arrangement::Arrangement, AppError> {
    let a = arrangement::find_by_id(&state.db, arr_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if a.organization_id != org_id {
        return Err(AppError::NotFound);
    }
    Ok(a)
}

/// Verify a voice belongs to the (org-scoped) arrangement and is live.
async fn scope_voice(state: &AppState, arr_id: Uuid, voice_id: Uuid) -> Result<(), AppError> {
    let v = voice::find_by_id(&state.db, voice_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if v.arrangement_id != arr_id {
        return Err(AppError::NotFound);
    }
    Ok(())
}

fn file_error_to_app_error(err: file::FileError) -> AppError {
    match err {
        file::FileError::Database(e) => AppError::Database(e),
        file::FileError::Duplicate => {
            AppError::Conflict("a file with this name and format already exists here".to_string())
        }
        file::FileError::UnsupportedMime => {
            AppError::UnsupportedMediaType("unsupported or unrecognized mime type".to_string())
        }
        file::FileError::ReplaceTargetGone => AppError::NotFound,
    }
}

fn storage_error_to_app_error(err: StorageError) -> AppError {
    match err {
        StorageError::TooLarge { limit } => AppError::PayloadTooLarge(format!(
            "upload exceeds the maximum allowed size of {limit} bytes"
        )),
        StorageError::NotFound => AppError::NotFound,
        StorageError::InvalidRange => {
            AppError::RangeNotSatisfiable("the requested Range cannot be satisfied".to_string())
        }
        StorageError::Upstream | StorageError::S3(_) => {
            AppError::Internal(anyhow::anyhow!("object storage error: {err}"))
        }
    }
}

fn field_report(field: &str, msg: &str) -> garde::Report {
    let mut report = garde::Report::new();
    report.append(garde::Path::new(field), garde::Error::new(msg.to_string()));
    report
}

/// Strip any directory components and trailing extension from an uploaded
/// filename to get `File.name`. Rejects empty results.
fn name_stem(file_name: &str) -> Option<String> {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    let stem = stem.trim();
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

/// Adapt an axum multipart `Field` to the storage layer's [`ChunkSource`].
impl storage::ChunkSource for axum::extract::multipart::Field<'_> {
    async fn next_chunk(&mut self) -> Result<Option<axum::body::Bytes>, StorageError> {
        self.chunk().await.map_err(|_| StorageError::Upstream)
    }
}

// ---------------------------------------------------------------------------
// Upload (POST) — shared logic for voice and score files.
// ---------------------------------------------------------------------------

async fn do_upload(
    state: AppState,
    auth: BearerOrSession,
    request_id: Uuid,
    org_id: Uuid,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;
    scope_arrangement(&state, org_id, arr_id).await?;
    if let Some(vid) = voice_id {
        scope_voice(&state, arr_id, vid).await?;
    }

    // Find the first multipart part that carries a file (has a filename).
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::Validation(field_report("file", "malformed multipart body")))?
    {
        let Some(file_name) = field.file_name().map(str::to_string) else {
            continue;
        };
        let mime = field
            .content_type()
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let (format, ext) = file::format_and_ext_for_mime(&mime).ok_or_else(|| {
            AppError::UnsupportedMediaType(format!("mime type '{mime}' is not a stored format"))
        })?;
        let name = name_stem(&file_name)
            .ok_or_else(|| AppError::Validation(field_report("file", "missing filename")))?;

        let slugs = file::resolve_path_slugs(&state.db, arr_id, voice_id)
            .await?
            .ok_or(AppError::NotFound)?;
        let key = file::derived_key(
            &slugs.org_slug,
            &slugs.arrangement_slug,
            slugs.voice_slug.as_deref(),
            &name,
            ext,
        );

        // DB row first (cheap duplicate detection), then stream the bytes.
        // CLAUDE.md backup ordering: a DB ref to a not-yet-written object is
        // the safe (detectable-404) failure mode.
        let id = Uuid::now_v7();
        let created = file::create(
            &state.db,
            id,
            file::NewFile {
                arrangement_id: arr_id,
                voice_id,
                name: &name,
                format,
                mime_type: &mime,
                derived_from_file_id: None,
                conversion_quality: None,
                created_by: Some(auth.user.id),
            },
        )
        .await
        .map_err(file_error_to_app_error)?;

        match storage::upload_streaming(
            &state.s3,
            &state.config.s3_bucket,
            &key,
            &mime,
            state.config.max_upload_bytes,
            field,
        )
        .await
        {
            Ok(bytes) => {
                audit(
                    &state.db,
                    &AuditContext {
                        actor_user_id: Some(auth.user.id),
                        org_id: Some(org_id),
                        request_id: Some(request_id),
                    },
                    "file.create",
                    "file",
                    Some(id),
                    serde_json::json!({ "name": name, "format": format, "key": key, "bytes": bytes }),
                )
                .await;
                return Ok((StatusCode::CREATED, Json(FileResponse::from(created))).into_response());
            }
            Err(e) => {
                // Roll back the orphaned row so a failed/oversize upload leaves
                // no dangling DB reference.
                let _ = file::soft_delete(&state.db, id).await;
                return Err(storage_error_to_app_error(e));
            }
        }
    }

    Err(AppError::Validation(field_report(
        "file",
        "no file part in the multipart body",
    )))
}

async fn upload_score_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    multipart: Multipart,
) -> Result<Response, AppError> {
    do_upload(state, auth, request_id, org_id, arr_id, None, multipart).await
}

async fn upload_voice_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arr_id, voice_id)): Path<(Uuid, Uuid, Uuid)>,
    multipart: Multipart,
) -> Result<Response, AppError> {
    do_upload(
        state,
        auth,
        request_id,
        org_id,
        arr_id,
        Some(voice_id),
        multipart,
    )
    .await
}

// ---------------------------------------------------------------------------
// Download (GET one) — streamed, Range-aware.
// ---------------------------------------------------------------------------

async fn do_download(
    state: AppState,
    auth: BearerOrSession,
    org_id: Uuid,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
    file_id: Uuid,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    scope_arrangement(&state, org_id, arr_id).await?;

    let f = file::find_by_id(&state.db, file_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if f.arrangement_id != arr_id || f.voice_id != voice_id {
        return Err(AppError::NotFound);
    }

    let slugs = file::resolve_path_slugs(&state.db, arr_id, voice_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let (_format, ext) = file::format_and_ext_for_mime(&f.mime_type)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("stored file has unknown mime")))?;
    let key = file::derived_key(
        &slugs.org_slug,
        &slugs.arrangement_slug,
        slugs.voice_slug.as_deref(),
        &f.name,
        ext,
    );

    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let obj = storage::get_object(&state.s3, &state.config.s3_bucket, &key, range.as_deref())
        .await
        .map_err(storage_error_to_app_error)?;

    let status = if obj.content_range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let reader = obj.body.into_async_read();
    let stream = tokio_util::io::ReaderStream::new(reader);
    let mut response = Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, f.mime_type.clone());

    if let Some(len) = obj.content_length {
        response = response.header(header::CONTENT_LENGTH, len);
    }
    if let Some(cr) = obj.content_range {
        response = response.header(header::CONTENT_RANGE, cr);
    }

    response
        .body(Body::from_stream(stream))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to build response: {e}")))
}

async fn download_score_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arr_id, file_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    do_download(state, auth, org_id, arr_id, None, file_id, headers).await
}

async fn download_voice_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arr_id, voice_id, file_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    do_download(
        state,
        auth,
        org_id,
        arr_id,
        Some(voice_id),
        file_id,
        headers,
    )
    .await
}

// ---------------------------------------------------------------------------
// List (GET many).
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn do_list(
    state: AppState,
    auth: BearerOrSession,
    org_id: Uuid,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
    q: ListQuery,
) -> Result<Json<Page<FileResponse>>, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Musician).await?;
    scope_arrangement(&state, org_id, arr_id).await?;
    if let Some(vid) = voice_id {
        scope_voice(&state, arr_id, vid).await?;
    }

    let (limit, offset) = PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);

    let (items, total) = file::list(
        &state.db,
        arr_id,
        voice_id,
        i64::from(limit),
        i64::from(offset),
    )
    .await?;

    Ok(Json(Page {
        items: items.into_iter().map(FileResponse::from).collect(),
        total,
        limit,
        offset,
    }))
}

async fn list_score_files(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Page<FileResponse>>, AppError> {
    do_list(state, auth, org_id, arr_id, None, q).await
}

async fn list_voice_files(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arr_id, voice_id)): Path<(Uuid, Uuid, Uuid)>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Page<FileResponse>>, AppError> {
    do_list(state, auth, org_id, arr_id, Some(voice_id), q).await
}

// ---------------------------------------------------------------------------
// Replace (PUT) — new row + soft-delete old in one txn; bytes overwrite the
// same derived key (MinIO versions the content).
// ---------------------------------------------------------------------------

// Internal shared helper with a deliberately flat path/auth parameter list;
// the two thin axum wrappers below supply them from the extracted path.
#[allow(clippy::too_many_arguments)]
async fn do_replace(
    state: AppState,
    auth: BearerOrSession,
    request_id: Uuid,
    org_id: Uuid,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
    file_id: Uuid,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;
    scope_arrangement(&state, org_id, arr_id).await?;

    let old = file::find_by_id(&state.db, file_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if old.arrangement_id != arr_id || old.voice_id != voice_id {
        return Err(AppError::NotFound);
    }

    let slugs = file::resolve_path_slugs(&state.db, arr_id, voice_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let (old_format, ext) = file::format_and_ext_for_mime(&old.mime_type)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("stored file has unknown mime")))?;
    // Replacement keeps identity (same name/format/voice) → same derived key.
    let key = file::derived_key(
        &slugs.org_slug,
        &slugs.arrangement_slug,
        slugs.voice_slug.as_deref(),
        &old.name,
        ext,
    );

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::Validation(field_report("file", "malformed multipart body")))?
    {
        if field.file_name().is_none() {
            continue;
        }

        // A replacement must keep the same format + extension (so the derived
        // key, and what we serve, stay coherent). A different format is a new
        // file, not a replace — reject rather than store mismatched bytes.
        let new_mime = field
            .content_type()
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let new_pair = file::format_and_ext_for_mime(&new_mime).ok_or_else(|| {
            AppError::UnsupportedMediaType(format!("mime type '{new_mime}' is not a stored format"))
        })?;
        if new_pair != (old_format, ext) {
            return Err(AppError::Conflict(format!(
                "a replacement must keep the original's format ('{old_format}'); \
                 upload a new file instead"
            )));
        }

        // Swap the DB identity FIRST (new row + soft-delete old, one txn),
        // then write the bytes to the (identical) key. If the upload fails we
        // compensate by restoring the old row — so a partial failure never
        // leaves the old identity pointing at the new bytes. This matches the
        // create path's DB-first ordering and CLAUDE.md's safe-failure rule.
        let new_id = Uuid::now_v7();
        let new = file::replace(
            &state.db,
            old.id,
            new_id,
            file::NewFile {
                arrangement_id: arr_id,
                voice_id,
                name: &old.name,
                format: &old.format,
                mime_type: &old.mime_type,
                // Preserve the derivation chain across the identity swap.
                derived_from_file_id: old.derived_from_file_id,
                conversion_quality: old.conversion_quality.as_deref(),
                created_by: Some(auth.user.id),
            },
        )
        .await
        .map_err(file_error_to_app_error)?;

        let bytes = match storage::upload_streaming(
            &state.s3,
            &state.config.s3_bucket,
            &key,
            &old.mime_type,
            state.config.max_upload_bytes,
            field,
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                // Restore the pre-replace state: drop the new row, re-live old.
                let _ = file::restore_replaced(&state.db, old.id, new_id).await;
                return Err(storage_error_to_app_error(e));
            }
        };

        audit(
            &state.db,
            &AuditContext {
                actor_user_id: Some(auth.user.id),
                org_id: Some(org_id),
                request_id: Some(request_id),
            },
            "file.replace",
            "file",
            Some(new_id),
            serde_json::json!({ "replaced": old.id, "name": old.name, "key": key, "bytes": bytes }),
        )
        .await;

        return Ok((StatusCode::OK, Json(FileResponse::from(new))).into_response());
    }

    Err(AppError::Validation(field_report(
        "file",
        "no file part in the multipart body",
    )))
}

async fn replace_score_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arr_id, file_id)): Path<(Uuid, Uuid, Uuid)>,
    multipart: Multipart,
) -> Result<Response, AppError> {
    do_replace(
        state, auth, request_id, org_id, arr_id, None, file_id, multipart,
    )
    .await
}

async fn replace_voice_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arr_id, voice_id, file_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
    multipart: Multipart,
) -> Result<Response, AppError> {
    do_replace(
        state,
        auth,
        request_id,
        org_id,
        arr_id,
        Some(voice_id),
        file_id,
        multipart,
    )
    .await
}

// ---------------------------------------------------------------------------
// Delete (DELETE) — soft-delete; MinIO object retained.
// ---------------------------------------------------------------------------

async fn do_delete(
    state: AppState,
    auth: BearerOrSession,
    request_id: Uuid,
    org_id: Uuid,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
    file_id: Uuid,
) -> Result<StatusCode, AppError> {
    require_org_role_v1(&state, &auth, org_id, Role::Archivist).await?;
    scope_arrangement(&state, org_id, arr_id).await?;

    let f = file::find_by_id(&state.db, file_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if f.arrangement_id != arr_id || f.voice_id != voice_id {
        return Err(AppError::NotFound);
    }

    let deleted = file::soft_delete(&state.db, file_id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: Some(org_id),
            request_id: Some(request_id),
        },
        "file.soft_delete",
        "file",
        Some(file_id),
        serde_json::json!({ "name": f.name, "format": f.format }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

async fn delete_score_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arr_id, file_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    do_delete(state, auth, request_id, org_id, arr_id, None, file_id).await
}

async fn delete_voice_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arr_id, voice_id, file_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    do_delete(
        state,
        auth,
        request_id,
        org_id,
        arr_id,
        Some(voice_id),
        file_id,
    )
    .await
}
