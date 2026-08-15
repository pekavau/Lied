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
use axum::Json;
use serde::Serialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::authz::require_org_role_v1;
use crate::auth::extractors::BearerOrSession;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::membership::Role;
use crate::domain::{arrangement, file, voice};
use crate::error::AppError;
use crate::file_service;
use crate::pagination::{Page, PageParams};
use crate::routes::openapi::{
    CommonErrors, Conflict409, Forbidden403, NotFound404, PayloadTooLarge413, Precondition412,
    Range416, UnsupportedMedia415,
};
use crate::routes::RequestId;
use crate::state::AppState;
use crate::storage::{self, StorageError};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_score_files, upload_score_file))
        .routes(routes!(
            download_score_file,
            replace_score_file,
            delete_score_file
        ))
        .routes(routes!(list_voice_files, upload_voice_file))
        .routes(routes!(
            download_voice_file,
            replace_voice_file,
            delete_voice_file
        ))
        // Uploads stream to MinIO with a manual byte ceiling, so the default
        // 2 MiB extractor limit must be lifted on these routes only.
        .layer(DefaultBodyLimit::disable())
}

/// Metadata about a stored file (score or voice).
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileResponse {
    pub id: Uuid,
    pub arrangement_id: Uuid,
    pub voice_id: Option<Uuid>,
    pub name: String,
    pub format: String,
    pub mime_type: String,
    pub derived_from_file_id: Option<Uuid>,
    pub conversion_quality: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
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

/// Multipart upload body: a single `file` part with a `filename` and a
/// recognised `Content-Type`.
///
/// Documented here as the OpenAPI `requestBody` schema for upload/replace
/// endpoints; the actual extraction is done by axum's [`Multipart`] extractor.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub struct UploadForm {
    /// The file bytes (multipart part named `file`).
    #[schema(format = Binary, content_media_type = "application/octet-stream")]
    file: String,
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

fn upload_error_to_app_error(err: file_service::UploadError) -> AppError {
    use file_service::UploadError as E;
    match err {
        E::UnsupportedMime(mime) => {
            AppError::UnsupportedMediaType(format!("mime type '{mime}' is not a stored format"))
        }
        // A corrupt stored row, not a client error: 500, not 415.
        E::StoredMimeUnknown(mime) => AppError::Internal(anyhow::anyhow!(
            "stored file has an unrecognized mime type '{mime}'"
        )),
        E::EmptyName => AppError::Validation(field_report("file", "missing filename")),
        E::FormatMismatch { expected } => AppError::Conflict(format!(
            "a replacement must keep the original's format {expected}; upload a new file instead"
        )),
        E::Duplicate => {
            AppError::Conflict("a file with this name and format already exists here".to_string())
        }
        E::NotFound | E::ReplaceTargetGone => AppError::NotFound,
        E::TooLarge { limit } => AppError::PayloadTooLarge(format!(
            "upload exceeds the maximum allowed size of {limit} bytes"
        )),
        E::Db(e) => AppError::Database(e),
        E::Storage(message) => {
            AppError::Internal(anyhow::anyhow!("object storage error: {message}"))
        }
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

    // Find the first multipart part that carries a file (has a filename), then
    // hand it to the shared store service (same logic as the /admin console).
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

        let stored = file_service::store_upload(
            &state,
            arr_id,
            voice_id,
            &file_name,
            &mime,
            Some(auth.user.id),
            field,
        )
        .await
        .map_err(upload_error_to_app_error)?;

        audit(
            &state.db,
            &AuditContext {
                actor_user_id: Some(auth.user.id),
                org_id: Some(org_id),
                request_id: Some(request_id),
            },
            "file.create",
            "file",
            Some(stored.file.id),
            serde_json::json!({
                "name": stored.file.name,
                "format": stored.file.format,
                "key": stored.key,
                "bytes": stored.bytes,
            }),
        )
        .await;
        return Ok((StatusCode::CREATED, Json(FileResponse::from(stored.file))).into_response());
    }

    Err(AppError::Validation(field_report(
        "file",
        "no file part in the multipart body",
    )))
}

/// Upload a full-score file to an arrangement (owner/archivist only).
///
/// The request body must be `multipart/form-data` with a single part that
/// carries a recognised `Content-Type` (see the format table in CLAUDE.md).
/// The file is streamed directly to MinIO — server memory per upload is a
/// fixed chunk buffer, not proportional to file size.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/arrangements/{arrId}/files",
    tag = "files",
    summary = "Upload a full-score file",
    params(
        ("orgId"  = Uuid, Path, description = "Organization ID"),
        ("arrId"  = Uuid, Path, description = "Arrangement ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body(content = UploadForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "File uploaded", body = FileResponse),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Conflict409,
        UnsupportedMedia415,
        PayloadTooLarge413,
    )
)]
async fn upload_score_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    multipart: Multipart,
) -> Result<Response, AppError> {
    do_upload(state, auth, request_id, org_id, arr_id, None, multipart).await
}

/// Upload a voice file (owner/archivist only).
///
/// The request body must be `multipart/form-data` with a single part that
/// carries a recognised `Content-Type`.
#[utoipa::path(
    post,
    path = "/orgs/{orgId}/arrangements/{arrId}/voices/{voiceId}/files",
    tag = "files",
    summary = "Upload a voice file",
    params(
        ("orgId"    = Uuid, Path, description = "Organization ID"),
        ("arrId"    = Uuid, Path, description = "Arrangement ID"),
        ("voiceId"  = Uuid, Path, description = "Voice ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body(content = UploadForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "File uploaded", body = FileResponse),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Conflict409,
        UnsupportedMedia415,
        PayloadTooLarge413,
    )
)]
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

/// Download a full-score file (requires `musician` role). Supports HTTP `Range`.
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{arrId}/files/{fileId}",
    tag = "files",
    summary = "Download a full-score file",
    params(
        ("orgId"   = Uuid,   Path,   description = "Organization ID"),
        ("arrId"   = Uuid,   Path,   description = "Arrangement ID"),
        ("fileId"  = Uuid,   Path,   description = "File ID"),
        ("Range"   = Option<String>, Header,
            description = "Byte-range request (e.g. `bytes=0-1023`). Returns 206 Partial Content."),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Full file body",
            content_type = "application/octet-stream",
            headers(
                ("Content-Type"   = String,  description = "MIME type of the stored file"),
                ("Accept-Ranges"  = String,  description = "Always `bytes`"),
                ("Content-Length" = u64,     description = "File size in bytes"),
            )),
        (status = 206, description = "Partial content (Range satisfied)",
            content_type = "application/octet-stream",
            headers(("Content-Range" = String, description = "Satisfied byte range"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Range416,
    )
)]
async fn download_score_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arr_id, file_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    do_download(state, auth, org_id, arr_id, None, file_id, headers).await
}

/// Download a voice file (requires `musician` role). Supports HTTP `Range`.
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{arrId}/voices/{voiceId}/files/{fileId}",
    tag = "files",
    summary = "Download a voice file",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("arrId"    = Uuid,   Path,   description = "Arrangement ID"),
        ("voiceId"  = Uuid,   Path,   description = "Voice ID"),
        ("fileId"   = Uuid,   Path,   description = "File ID"),
        ("Range"    = Option<String>, Header,
            description = "Byte-range request (e.g. `bytes=0-1023`). Returns 206 Partial Content."),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Full file body",
            content_type = "application/octet-stream",
            headers(
                ("Content-Type"   = String, description = "MIME type of the stored file"),
                ("Accept-Ranges"  = String, description = "Always `bytes`"),
                ("Content-Length" = u64,    description = "File size in bytes"),
            )),
        (status = 206, description = "Partial content (Range satisfied)",
            content_type = "application/octet-stream",
            headers(("Content-Range" = String, description = "Satisfied byte range"))),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Range416,
    )
)]
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

/// List full-score files for an arrangement (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{arrId}/files",
    tag = "files",
    summary = "List full-score files",
    params(
        ("orgId"  = Uuid, Path, description = "Organization ID"),
        ("arrId"  = Uuid, Path, description = "Arrangement ID"),
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated file list",
            body = inline(Page<FileResponse>)),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
async fn list_score_files(
    auth: BearerOrSession,
    State(state): State<AppState>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Page<FileResponse>>, AppError> {
    do_list(state, auth, org_id, arr_id, None, q).await
}

/// List voice files for a specific voice (requires `musician` role).
#[utoipa::path(
    get,
    path = "/orgs/{orgId}/arrangements/{arrId}/voices/{voiceId}/files",
    tag = "files",
    summary = "List voice files",
    params(
        ("orgId"    = Uuid, Path, description = "Organization ID"),
        ("arrId"    = Uuid, Path, description = "Arrangement ID"),
        ("voiceId"  = Uuid, Path, description = "Voice ID"),
        ("limit"    = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset"   = Option<u32>, Query, description = "Page offset"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated file list",
            body = inline(Page<FileResponse>)),
        CommonErrors,
        Forbidden403,
        NotFound404,
    )
)]
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

    // Find the file part and hand it to the shared store service (the identity
    // swap, format/extension check, provenance preservation, and rollback all
    // live there — same logic as the /admin console).
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::Validation(field_report("file", "malformed multipart body")))?
    {
        if field.file_name().is_none() {
            continue;
        }
        let new_mime = field
            .content_type()
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let stored =
            file_service::store_replacement(&state, &old, &new_mime, Some(auth.user.id), field)
                .await
                .map_err(upload_error_to_app_error)?;

        audit(
            &state.db,
            &AuditContext {
                actor_user_id: Some(auth.user.id),
                org_id: Some(org_id),
                request_id: Some(request_id),
            },
            "file.replace",
            "file",
            Some(stored.file.id),
            serde_json::json!({
                "replaced": old.id,
                "name": old.name,
                "key": stored.key,
                "bytes": stored.bytes,
            }),
        )
        .await;

        return Ok((StatusCode::OK, Json(FileResponse::from(stored.file))).into_response());
    }

    Err(AppError::Validation(field_report(
        "file",
        "no file part in the multipart body",
    )))
}

/// Replace a full-score file in-place (owner/archivist; requires `If-Match`).
///
/// The replacement must carry the same `Content-Type` as the original. A
/// format change requires uploading a new file instead. The MinIO object key
/// is unchanged; MinIO versioning captures the content history.
#[utoipa::path(
    put,
    path = "/orgs/{orgId}/arrangements/{arrId}/files/{fileId}",
    tag = "files",
    summary = "Replace a full-score file",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("arrId"    = Uuid,   Path,   description = "Arrangement ID"),
        ("fileId"   = Uuid,   Path,   description = "File ID to replace"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body(content = UploadForm, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "File replaced", body = FileResponse),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Conflict409,
        Precondition412,
        UnsupportedMedia415,
        PayloadTooLarge413,
    )
)]
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

/// Replace a voice file in-place (owner/archivist; requires `If-Match`).
#[utoipa::path(
    put,
    path = "/orgs/{orgId}/arrangements/{arrId}/voices/{voiceId}/files/{fileId}",
    tag = "files",
    summary = "Replace a voice file",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("arrId"    = Uuid,   Path,   description = "Arrangement ID"),
        ("voiceId"  = Uuid,   Path,   description = "Voice ID"),
        ("fileId"   = Uuid,   Path,   description = "File ID to replace"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    request_body(content = UploadForm, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "File replaced", body = FileResponse),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Conflict409,
        Precondition412,
        UnsupportedMedia415,
        PayloadTooLarge413,
    )
)]
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

/// Soft-delete a full-score file (owner/archivist; requires `If-Match`).
///
/// The MinIO object is retained; the DB row is hidden from listings. The
/// object can be recovered via a future admin hard-delete / undelete path.
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/arrangements/{arrId}/files/{fileId}",
    tag = "files",
    summary = "Soft-delete a full-score file",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("arrId"    = Uuid,   Path,   description = "Arrangement ID"),
        ("fileId"   = Uuid,   Path,   description = "File ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "File soft-deleted"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
async fn delete_score_file(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path((org_id, arr_id, file_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    do_delete(state, auth, request_id, org_id, arr_id, None, file_id).await
}

/// Soft-delete a voice file (owner/archivist; requires `If-Match`).
#[utoipa::path(
    delete,
    path = "/orgs/{orgId}/arrangements/{arrId}/voices/{voiceId}/files/{fileId}",
    tag = "files",
    summary = "Soft-delete a voice file",
    params(
        ("orgId"    = Uuid,   Path,   description = "Organization ID"),
        ("arrId"    = Uuid,   Path,   description = "Arrangement ID"),
        ("voiceId"  = Uuid,   Path,   description = "Voice ID"),
        ("fileId"   = Uuid,   Path,   description = "File ID"),
        ("If-Match" = String, Header, description = "ETag from a prior GET; stale → 412"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "File soft-deleted"),
        CommonErrors,
        Forbidden403,
        NotFound404,
        Precondition412,
    )
)]
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

#[cfg(test)]
mod tests {
    use super::upload_error_to_app_error;
    use crate::file_service::UploadError;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    fn status_of(err: UploadError) -> StatusCode {
        upload_error_to_app_error(err).into_response().status()
    }

    #[test]
    fn a_bad_upload_blames_the_client_but_a_corrupt_stored_row_does_not() {
        // The client sent a media type we don't store: their problem, 415.
        assert_eq!(
            status_of(UploadError::UnsupportedMime("text/plain".to_string())),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        // The *stored* row carries a mime that isn't in the lookup table — it
        // passed that same table on write, so this is our data bug. Reporting
        // 415 would blame the caller for a media type they never sent.
        assert_eq!(
            status_of(UploadError::StoredMimeUnknown(
                "application/whatever".to_string()
            )),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn the_remaining_store_failures_keep_their_documented_statuses() {
        assert_eq!(
            status_of(UploadError::EmptyName),
            StatusCode::BAD_REQUEST,
            "a nameless part is a malformed request"
        );
        assert_eq!(
            status_of(UploadError::FormatMismatch {
                expected: "pdf (.pdf)".to_string()
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(status_of(UploadError::Duplicate), StatusCode::CONFLICT);
        assert_eq!(status_of(UploadError::NotFound), StatusCode::NOT_FOUND);
        assert_eq!(
            status_of(UploadError::ReplaceTargetGone),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(UploadError::TooLarge { limit: 1024 }),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            status_of(UploadError::Storage("connection reset".to_string())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
