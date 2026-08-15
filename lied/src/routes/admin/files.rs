//! `/admin/orgs/{org}/arrangements/{arr}/files` and `…/voices/{voice}/files`
//! — file management for the console (Phase 2, issue #32). HTMX/maud over the
//! phase-1 file domain + streaming storage; conversion *actions* stay deferred.
//!
//! Score files (`voice_id = None`) and voice files share every handler via an
//! `Option<Uuid>` voice id. Authorization: any staff may view the file lists;
//! only `owner`/`archivist` may upload/replace/delete (conductor read-only),
//! per the permission matrix.
//!
//! **Uploads/replaces are `multipart/form-data`** (the body *is* the streamed
//! file), so they can't carry the CSRF token in a urlencoded field and the
//! JS-free UI can't set the `HX-CSRF` header. Instead the form action carries
//! the session CSRF token in a `?csrf=…` query param, which the CSRF
//! middleware validates without touching the streaming body (issue #32). The
//! streaming path itself is unchanged — bytes flow straight to MinIO, bounded
//! by the chunk buffer, not the file size.

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream::StreamExt;
use maud::html;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::audit_log::audit;
use crate::domain::file;
use crate::file_service;
use crate::routes::admin::arrangements::{
    audit_ctx, csrf_token, error_page, require_found, scoped_arrangement, scoped_voice,
};
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;
use crate::storage;

pub fn router() -> Router<AppState> {
    // Streaming routes: uploads/replaces lift the default 2 MiB body limit so
    // bytes flow to MinIO bounded by the chunk buffer, not fully buffered. The
    // GET list pages share the upload path; a disabled limit on a body-less GET
    // is harmless. The limit stays scoped here — delete/undelete (urlencoded
    // POSTs) keep the default guard.
    let streaming = Router::new()
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files",
            get(score_files_page).post(upload_score),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files/:file_id/replace",
            post(replace_score),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files",
            get(voice_files_page).post(upload_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files/:file_id/replace",
            post(replace_voice),
        )
        .layer(axum::extract::DefaultBodyLimit::disable());

    Router::new()
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files/:file_id/delete",
            post(delete_score),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files/:file_id/undelete",
            post(undelete_score),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files/:file_id/delete",
            post(delete_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files/:file_id/undelete",
            post(undelete_voice),
        )
        .merge(streaming)
}

/// Where a file screen / write is scoped: the arrangement, and — for voice
/// files — a specific voice under it. Bundles the ids + the base URL so the
/// two variants share every handler.
struct Target {
    org_id: Uuid,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
}

impl Target {
    /// The console URL for this file collection.
    fn base_url(&self) -> String {
        match self.voice_id {
            Some(vid) => format!(
                "/admin/orgs/{}/arrangements/{}/voices/{vid}/files",
                self.org_id, self.arr_id
            ),
            None => format!(
                "/admin/orgs/{}/arrangements/{}/files",
                self.org_id, self.arr_id
            ),
        }
    }

    /// The `/v1` download URL for a file (a GET on the file resource — session
    /// cookie authed, so a browser link works).
    fn download_url(&self, file_id: Uuid) -> String {
        match self.voice_id {
            Some(vid) => format!(
                "/v1/orgs/{}/arrangements/{}/voices/{vid}/files/{file_id}",
                self.org_id, self.arr_id
            ),
            None => format!(
                "/v1/orgs/{}/arrangements/{}/files/{file_id}",
                self.org_id, self.arr_id
            ),
        }
    }
}

/// Resolve login + console context + verify the arrangement (and voice) belong
/// to the org, returning a ready response on any failure.
async fn enter_target(
    state: &AppState,
    auth: Option<AuthSession>,
    org_id: Uuid,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
) -> Result<(ConsoleCtx, Target), Response> {
    let ctx = console::enter(state, auth, org_id).await?;
    require_found(
        scoped_arrangement(state, &ctx, arr_id, false).await,
        &ctx,
        Section::Arrangements,
        "Arrangement not found.",
    )?;
    if let Some(vid) = voice_id {
        require_found(
            scoped_voice(state, &ctx, arr_id, vid, false).await,
            &ctx,
            Section::Arrangements,
            "Voice not found.",
        )?;
    }
    Ok((
        ctx,
        Target {
            org_id,
            arr_id,
            voice_id,
        },
    ))
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

async fn files_page(
    state: AppState,
    auth: Option<AuthSession>,
    target: (Uuid, Uuid, Option<Uuid>),
    session: tower_sessions::Session,
) -> Response {
    let (org_id, arr_id, voice_id) = target;
    let (ctx, target) = match enter_target(&state, auth, org_id, arr_id, voice_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    if !ctx.is_staff() {
        return console::section_forbidden(&ctx);
    }
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };
    let can_edit = ctx.can_edit_arrangements();

    // One screenful is the operator-configured page ceiling (CLAUDE.md: every
    // limit is documented and configurable — never a literal here). The total
    // is kept so a list longer than the page says so instead of silently
    // truncating.
    let page_size = i64::from(state.config.max_page_size);
    let (files, total) = match file::list(&state.db, arr_id, voice_id, page_size, 0).await {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "failed to list files");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load files.",
            );
        }
    };
    let (deleted, deleted_total) = if can_edit {
        // Bounded by the same ceiling: a heavily-replaced file accumulates one
        // soft-deleted row per replace, forever.
        file::list_deleted(&state.db, arr_id, voice_id, page_size, 0)
            .await
            .unwrap_or_default()
    } else {
        (Vec::new(), 0)
    };
    let sizes = collect_sizes(&state, arr_id, voice_id, &files).await;

    let heading = match target.voice_id {
        Some(_) => "Voice files",
        None => "Score files",
    };
    let base = target.base_url();
    let body = html! {
        h2 { (heading) }
        @if !can_edit { p class="muted" { "You have read-only access to files." } }
        p class="muted" {
            "Files are stored as-is; format and provenance are shown but no "
            "conversion runs on them (Supported Formats)."
        }
        table {
            thead { tr {
                th { "Name" } th { "Format" } th { "MIME type" } th { "Size" }
                th { "Quality" } th { "Uploaded" } th {}
            } }
            tbody {
                @for f in &files {
                    tr {
                        td { (f.name) }
                        td { (f.format) }
                        td { (f.mime_type) }
                        td { @match sizes.get(&f.id) { Some(n) => (human_size(*n)), None => "?" } }
                        td {
                            @match &f.conversion_quality {
                                Some(q) => (q),
                                None => span class="muted" { "source" },
                            }
                            @if f.derived_from_file_id.is_some() { " (derived)" }
                        }
                        td { (f.created_at.format("%Y-%m-%d %H:%M")) }
                        td {
                            a href=(target.download_url(f.id)) { "Download" }
                            @if can_edit {
                                " "
                                form class="inline" method="post" enctype="multipart/form-data"
                                     action=(format!("{base}/{}/replace?csrf={token}", f.id)) {
                                    input type="file" name="file" required;
                                    button type="submit" { "Replace" }
                                }
                                " "
                                form class="inline" method="post" action=(format!("{base}/{}/delete", f.id)) {
                                    (layout::csrf_field(&token))
                                    button type="submit" { "Delete" }
                                }
                            }
                        }
                    }
                }
            }
        }
        @if files.is_empty() { p class="muted" { "No files yet." } }
        @if total > files.len() as i64 {
            p class="muted" {
                "Showing the first " (files.len()) " of " (total) " files "
                "(the server's page limit)."
            }
        }

        @if can_edit {
            h2 { "Upload a file" }
            // Multipart body → CSRF token travels in the action query string.
            form method="post" enctype="multipart/form-data" action=(format!("{base}?csrf={token}")) {
                input type="file" name="file" required;
                button type="submit" { "Upload" }
            }
            p class="muted" {
                "Accepted: LilyPond, MusicXML, PDF, and images. Max upload size "
                "is set by the server (LIED_MAX_UPLOAD_BYTES)."
            }

            @if !deleted.is_empty() {
                h2 { "Previous / deleted versions" }
                @if deleted_total > deleted.len() as i64 {
                    p class="muted" {
                        "Showing the " (deleted.len()) " most recently deleted of "
                        (deleted_total) "."
                    }
                }
                table {
                    thead { tr { th { "Name" } th { "Format" } th { "Deleted" } th {} } }
                    tbody {
                        @for f in &deleted {
                            tr {
                                td { (f.name) }
                                td { (f.format) }
                                td { @if let Some(t) = f.deleted_at { (t.format("%Y-%m-%d %H:%M")) } }
                                td {
                                    form class="inline" method="post" action=(format!("{base}/{}/undelete", f.id)) {
                                        (layout::csrf_field(&token))
                                        button type="submit" { "Restore" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        p {
            @match target.voice_id {
                Some(vid) => a href=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}/voices/{vid}")) { "← Back to voice" },
                None => a href=(format!("/admin/orgs/{org_id}/arrangements/{arr_id}")) { "← Back to arrangement" },
            }
        }
    };
    Html(console::console_page(&ctx, Section::Arrangements, body).into_string()).into_response()
}

/// How many object HEADs a single file screen may have in flight. Bounded so a
/// full page doesn't open a request per row against the object store at once;
/// sizes are decoration, not the point of the screen.
const SIZE_LOOKUP_CONCURRENCY: usize = 8;

/// How long the whole size-lookup pass may take. Sizes are best-effort, so a
/// slow or unreachable object store renders "?" instead of stalling the page.
const SIZE_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Best-effort object sizes for a file list. The path slugs are identical for
/// every file (same arrangement/voice), so they're resolved once; each key is
/// derived from the in-hand row (no per-file DB refetch), and the MinIO HEADs
/// run concurrently but capped at [`SIZE_LOOKUP_CONCURRENCY`] and bounded by
/// [`SIZE_LOOKUP_TIMEOUT`]. A file with no HEAD result is simply absent from
/// the map (rendered "?").
async fn collect_sizes(
    state: &AppState,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
    files: &[file::File],
) -> std::collections::HashMap<Uuid, u64> {
    let Ok(Some(slugs)) = file::resolve_path_slugs(&state.db, arr_id, voice_id).await else {
        return std::collections::HashMap::new();
    };
    // Owned (id, key) pairs first: the lookup futures must not borrow `files`,
    // or the resulting stream isn't `'static` enough for the router's handler
    // bounds.
    let keys: Vec<(Uuid, String)> = files
        .iter()
        .filter_map(|f| {
            let (_format, ext) = file::format_and_ext_for_mime(&f.mime_type)?;
            Some((
                f.id,
                file::derived_key(
                    &slugs.org_slug,
                    &slugs.arrangement_slug,
                    slugs.voice_slug.as_deref(),
                    &f.name,
                    ext,
                ),
            ))
        })
        .collect();

    let lookups = futures_util::stream::iter(keys)
        .map(|(id, key)| async move {
            let size = storage::head_object(&state.s3, &state.config.s3_bucket, &key)
                .await
                .ok()
                .map(|h| h.size);
            (id, size)
        })
        .buffer_unordered(SIZE_LOOKUP_CONCURRENCY)
        .filter_map(|(id, size)| async move { size.map(|s| (id, s)) })
        .collect::<std::collections::HashMap<_, _>>();

    match tokio::time::timeout(SIZE_LOOKUP_TIMEOUT, lookups).await {
        Ok(sizes) => sizes,
        Err(_) => {
            tracing::warn!("object size lookup timed out; rendering the file list without sizes");
            std::collections::HashMap::new()
        }
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Upload / replace (multipart) + delete / undelete (urlencoded)
// ---------------------------------------------------------------------------

async fn do_upload(
    state: AppState,
    auth: Option<AuthSession>,
    ids: (Uuid, Uuid, Option<Uuid>),
    request_id: Uuid,
    mut multipart: Multipart,
) -> Response {
    let (org_id, arr_id, voice_id) = ids;
    let (ctx, target) = match enter_target(&state, auth, org_id, arr_id, voice_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "malformed multipart upload body");
                return error_page(
                    &ctx,
                    StatusCode::BAD_REQUEST,
                    "The upload body was malformed.",
                );
            }
        };
        let Some(file_name) = field.file_name().map(str::to_string) else {
            continue;
        };
        let mime = field
            .content_type()
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());

        return match file_service::store_upload(
            &state,
            arr_id,
            voice_id,
            &file_name,
            &mime,
            Some(ctx.user().id),
            field,
        )
        .await
        {
            Ok(stored) => {
                audit(
                    &state.db,
                    &audit_ctx(&ctx, request_id),
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
                Redirect::to(&target.base_url()).into_response()
            }
            Err(error) => upload_error_page(&ctx, error),
        };
    }
    error_page(&ctx, StatusCode::BAD_REQUEST, "No file was provided.")
}

/// Map a shared [`file_service::UploadError`] to a console error page.
fn upload_error_page(ctx: &ConsoleCtx, err: file_service::UploadError) -> Response {
    use file_service::UploadError as E;
    let (status, message) = match err {
        E::UnsupportedMime(mime) => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("'{mime}' is not a stored format — use LilyPond, MusicXML, PDF, or an image."),
        ),
        // Our stored row is wrong, not the operator's upload.
        E::StoredMimeUnknown(mime) => {
            tracing::error!(%mime, "stored file has an unrecognized mime type");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "This file's stored type is unrecognized, so it can't be replaced. \
                 Please report this — the file's metadata needs fixing."
                    .to_string(),
            )
        }
        E::EmptyName => (
            StatusCode::BAD_REQUEST,
            "The file needs a name.".to_string(),
        ),
        E::FormatMismatch { expected } => (
            StatusCode::CONFLICT,
            format!("The replacement must keep the original's format {expected}."),
        ),
        E::Duplicate => (
            StatusCode::CONFLICT,
            "A file with this name and format already exists here — use Replace instead."
                .to_string(),
        ),
        E::NotFound => (StatusCode::NOT_FOUND, "Arrangement not found.".to_string()),
        E::ReplaceTargetGone => (StatusCode::NOT_FOUND, "File not found.".to_string()),
        E::TooLarge { limit } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("That file is too large (limit {limit} bytes)."),
        ),
        E::Db(error) => {
            tracing::error!(%error, "file store db error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the file.".to_string(),
            )
        }
        E::Storage(error) => {
            tracing::error!(%error, "file store storage error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The upload failed.".to_string(),
            )
        }
    };
    error_page(ctx, status, &message)
}

async fn do_replace(
    state: AppState,
    auth: Option<AuthSession>,
    ids: (Uuid, Uuid, Option<Uuid>, Uuid),
    request_id: Uuid,
    mut multipart: Multipart,
) -> Response {
    let (org_id, arr_id, voice_id, file_id) = ids;
    let (ctx, target) = match enter_target(&state, auth, org_id, arr_id, voice_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    // The file must exist under this arrangement/voice.
    let old = match require_found(
        scoped_file(&state, arr_id, voice_id, file_id).await,
        &ctx,
        Section::Arrangements,
        "File not found.",
    ) {
        Ok(f) => f,
        Err(response) => return response,
    };

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "malformed multipart replace body");
                return error_page(
                    &ctx,
                    StatusCode::BAD_REQUEST,
                    "The upload body was malformed.",
                );
            }
        };
        if field.file_name().is_none() {
            continue;
        }
        let mime = field
            .content_type()
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());

        return match file_service::store_replacement(
            &state,
            &old,
            &mime,
            Some(ctx.user().id),
            field,
        )
        .await
        {
            Ok(stored) => {
                audit(
                    &state.db,
                    &audit_ctx(&ctx, request_id),
                    "file.replace",
                    "file",
                    Some(stored.file.id),
                    serde_json::json!({
                        "name": old.name,
                        "replaced": old.id,
                        "key": stored.key,
                        "bytes": stored.bytes,
                    }),
                )
                .await;
                Redirect::to(&target.base_url()).into_response()
            }
            Err(error) => upload_error_page(&ctx, error),
        };
    }
    error_page(&ctx, StatusCode::BAD_REQUEST, "No file was provided.")
}

async fn do_delete(
    state: AppState,
    auth: Option<AuthSession>,
    ids: (Uuid, Uuid, Option<Uuid>, Uuid),
    request_id: Uuid,
) -> Response {
    let (org_id, arr_id, voice_id, file_id) = ids;
    let (ctx, target) = match enter_target(&state, auth, org_id, arr_id, voice_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    if let Err(response) = require_found(
        scoped_file(&state, arr_id, voice_id, file_id).await,
        &ctx,
        Section::Arrangements,
        "File not found.",
    ) {
        return response;
    }
    match file::soft_delete(&state.db, file_id).await {
        Ok(true) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "file.soft_delete",
                "file",
                Some(file_id),
                serde_json::json!({}),
            )
            .await;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, "failed to soft-delete file");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not delete the file.",
            );
        }
    }
    Redirect::to(&target.base_url()).into_response()
}

async fn do_undelete(
    state: AppState,
    auth: Option<AuthSession>,
    ids: (Uuid, Uuid, Option<Uuid>, Uuid),
    request_id: Uuid,
) -> Response {
    let (org_id, arr_id, voice_id, file_id) = ids;
    let (ctx, target) = match enter_target(&state, auth, org_id, arr_id, voice_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    if !ctx.can_edit_arrangements() {
        return console::section_forbidden(&ctx);
    }
    // Scope against the (deleted) row — a DB error surfaces as 500, not a
    // misleading 404 (require_found builds the right response).
    if let Err(response) = require_found(
        scoped_file_including_deleted(&state, arr_id, voice_id, file_id).await,
        &ctx,
        Section::Arrangements,
        "File not found.",
    ) {
        return response;
    }
    match file::undelete(&state.db, file_id).await {
        Ok(true) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "file.undelete",
                "file",
                Some(file_id),
                serde_json::json!({}),
            )
            .await;
            Redirect::to(&target.base_url()).into_response()
        }
        Ok(false) => Redirect::to(&target.base_url()).into_response(),
        Err(file::FileError::Duplicate) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "A current file already holds that name and format — delete or replace it first.",
        ),
        Err(file::FileError::Superseded) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "A newer file has taken this file's storage slot, so restoring it would serve the wrong content.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to undelete file");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not restore the file.",
            )
        }
    }
}

/// Fetch a live file scoped to `arr_id`/`voice_id` (a file id from another
/// arrangement/voice/org must 404).
async fn scoped_file(
    state: &AppState,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
    file_id: Uuid,
) -> Result<Option<file::File>, sqlx::Error> {
    Ok(file::find_by_id(&state.db, file_id)
        .await?
        .filter(|f| f.arrangement_id == arr_id && f.voice_id == voice_id))
}

async fn scoped_file_including_deleted(
    state: &AppState,
    arr_id: Uuid,
    voice_id: Option<Uuid>,
    file_id: Uuid,
) -> Result<Option<file::File>, sqlx::Error> {
    Ok(file::find_by_id_including_deleted(&state.db, file_id)
        .await?
        .filter(|f| f.arrangement_id == arr_id && f.voice_id == voice_id))
}

// ---------------------------------------------------------------------------
// Route handlers: thin score/voice wrappers over the shared logic
// ---------------------------------------------------------------------------

async fn score_files_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    session: tower_sessions::Session,
) -> Response {
    files_page(state, auth, (org_id, arr_id, None), session).await
}

async fn voice_files_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id)): Path<(Uuid, Uuid, Uuid)>,
    session: tower_sessions::Session,
) -> Response {
    files_page(state, auth, (org_id, arr_id, Some(voice_id)), session).await
}

async fn upload_score(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    multipart: Multipart,
) -> Response {
    do_upload(state, auth, (org_id, arr_id, None), request_id, multipart).await
}

async fn upload_voice(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    multipart: Multipart,
) -> Response {
    do_upload(
        state,
        auth,
        (org_id, arr_id, Some(voice_id)),
        request_id,
        multipart,
    )
    .await
}

async fn replace_score(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, file_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    multipart: Multipart,
) -> Response {
    do_replace(
        state,
        auth,
        (org_id, arr_id, None, file_id),
        request_id,
        multipart,
    )
    .await
}

async fn replace_voice(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id, file_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    multipart: Multipart,
) -> Response {
    do_replace(
        state,
        auth,
        (org_id, arr_id, Some(voice_id), file_id),
        request_id,
        multipart,
    )
    .await
}

async fn delete_score(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, file_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    do_delete(state, auth, (org_id, arr_id, None, file_id), request_id).await
}

async fn delete_voice(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id, file_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    do_delete(
        state,
        auth,
        (org_id, arr_id, Some(voice_id), file_id),
        request_id,
    )
    .await
}

async fn undelete_score(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, file_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    do_undelete(state, auth, (org_id, arr_id, None, file_id), request_id).await
}

async fn undelete_voice(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, arr_id, voice_id, file_id)): Path<(Uuid, Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    do_undelete(
        state,
        auth,
        (org_id, arr_id, Some(voice_id), file_id),
        request_id,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
