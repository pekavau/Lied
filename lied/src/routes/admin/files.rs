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
use maud::html;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::audit_log::audit;
use crate::domain::file;
use crate::routes::admin::arrangements::{
    audit_ctx, csrf_token, error_page, require_found, scoped_arrangement, scoped_voice,
};
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;
use crate::storage;

pub fn router() -> Router<AppState> {
    Router::new()
        // Score files (voice_id = None).
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files",
            get(score_files_page).post(upload_score),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files/:file_id/replace",
            post(replace_score),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files/:file_id/delete",
            post(delete_score),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/files/:file_id/undelete",
            post(undelete_score),
        )
        // Voice files.
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files",
            get(voice_files_page).post(upload_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files/:file_id/replace",
            post(replace_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files/:file_id/delete",
            post(delete_voice),
        )
        .route(
            "/orgs/:org_id/arrangements/:arr_id/voices/:voice_id/files/:file_id/undelete",
            post(undelete_voice),
        )
        // Uploads/replaces stream to MinIO with a manual byte ceiling, so the
        // default 2 MiB extractor body limit must be lifted on this tree.
        .layer(axum::extract::DefaultBodyLimit::disable())
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

    let (files, _total) = match file::list(&state.db, arr_id, voice_id, 200, 0).await {
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
    let deleted = if can_edit {
        file::list_deleted(&state.db, arr_id, voice_id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    // Best-effort size per file (a HEAD to MinIO); rendered as "?" on failure.
    let mut sizes = std::collections::HashMap::new();
    for f in &files {
        if let Some(size) = file_size(&state, &f.id).await {
            sizes.insert(f.id, size);
        }
    }

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

/// Best-effort size of a file's stored object (a HEAD to MinIO).
async fn file_size(state: &AppState, file_id: &Uuid) -> Option<u64> {
    let f = file::find_by_id(&state.db, *file_id).await.ok()??;
    let slugs = file::resolve_path_slugs(&state.db, f.arrangement_id, f.voice_id)
        .await
        .ok()??;
    let (_format, ext) = file::format_and_ext_for_mime(&f.mime_type)?;
    let key = file::derived_key(
        &slugs.org_slug,
        &slugs.arrangement_slug,
        slugs.voice_slug.as_deref(),
        &f.name,
        ext,
    );
    storage::head_object(&state.s3, &state.config.s3_bucket, &key)
        .await
        .ok()
        .map(|h| h.size)
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

    while let Ok(Some(field)) = multipart.next_field().await {
        let Some(file_name) = field.file_name().map(str::to_string) else {
            continue;
        };
        let mime = field
            .content_type()
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let Some((format, ext)) = file::format_and_ext_for_mime(&mime) else {
            return error_page(
                &ctx,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                &format!(
                    "'{mime}' is not a stored format — use LilyPond, MusicXML, PDF, or an image."
                ),
            );
        };
        let Some((name, _)) = file::split_filename(&file_name) else {
            return error_page(&ctx, StatusCode::BAD_REQUEST, "The file needs a name.");
        };
        let name = name.to_string();

        let slugs = match file::resolve_path_slugs(&state.db, arr_id, voice_id).await {
            Ok(Some(s)) => s,
            _ => return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        };
        let key = file::derived_key(
            &slugs.org_slug,
            &slugs.arrangement_slug,
            slugs.voice_slug.as_deref(),
            &name,
            ext,
        );

        // DB row first (cheap duplicate detection), then stream the bytes.
        let id = Uuid::now_v7();
        let created =
            match file::create(
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
                    created_by: Some(ctx.user().id),
                },
            )
            .await
            {
                Ok(created) => created,
                Err(file::FileError::Duplicate) => return error_page(
                    &ctx,
                    StatusCode::CONFLICT,
                    "A file with this name and format already exists here — use Replace instead.",
                ),
                Err(error) => {
                    tracing::error!(%error, "failed to create file row");
                    return error_page(
                        &ctx,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Could not save the file.",
                    );
                }
            };

        return match storage::upload_streaming(
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
                    &audit_ctx(&ctx, request_id),
                    "file.create",
                    "file",
                    Some(created.id),
                    serde_json::json!({ "name": name, "format": format, "bytes": bytes }),
                )
                .await;
                Redirect::to(&target.base_url()).into_response()
            }
            Err(storage::StorageError::TooLarge { limit }) => {
                let _ = file::soft_delete(&state.db, id).await;
                error_page(
                    &ctx,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &format!("That file is too large (limit {} bytes).", limit),
                )
            }
            Err(error) => {
                let _ = file::soft_delete(&state.db, id).await;
                tracing::error!(%error, "upload failed");
                error_page(
                    &ctx,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "The upload failed.",
                )
            }
        };
    }
    error_page(&ctx, StatusCode::BAD_REQUEST, "No file was provided.")
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

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.file_name().is_none() {
            continue;
        }
        let mime = field
            .content_type()
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let Some((format, ext)) = file::format_and_ext_for_mime(&mime) else {
            return error_page(
                &ctx,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "That file type isn't a stored format.",
            );
        };
        // Replacement keeps the same identity (name + format) so the derived
        // key and unique slot stay stable — reject a format switch.
        if format != old.format {
            return error_page(
                &ctx,
                StatusCode::CONFLICT,
                &format!(
                    "The replacement must be the same format as the original ({}).",
                    old.format
                ),
            );
        }
        let slugs = match file::resolve_path_slugs(&state.db, arr_id, voice_id).await {
            Ok(Some(s)) => s,
            _ => return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found."),
        };
        let key = file::derived_key(
            &slugs.org_slug,
            &slugs.arrangement_slug,
            slugs.voice_slug.as_deref(),
            &old.name,
            ext,
        );

        // DB-first: swap the row, then overwrite the object; roll back on
        // upload failure so the old row stays live.
        let new_id = Uuid::now_v7();
        let created = match file::replace(
            &state.db,
            old.id,
            new_id,
            file::NewFile {
                arrangement_id: arr_id,
                voice_id,
                name: &old.name,
                format,
                mime_type: &mime,
                derived_from_file_id: None,
                conversion_quality: None,
                created_by: Some(ctx.user().id),
            },
        )
        .await
        {
            Ok(created) => created,
            Err(file::FileError::ReplaceTargetGone) => {
                return error_page(&ctx, StatusCode::NOT_FOUND, "File not found.")
            }
            Err(error) => {
                tracing::error!(%error, "failed to swap file row on replace");
                return error_page(
                    &ctx,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not replace the file.",
                );
            }
        };

        return match storage::upload_streaming(
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
                    &audit_ctx(&ctx, request_id),
                    "file.replace",
                    "file",
                    Some(created.id),
                    serde_json::json!({ "name": old.name, "replaced": old.id, "bytes": bytes }),
                )
                .await;
                Redirect::to(&target.base_url()).into_response()
            }
            Err(error) => {
                // Undo the row swap: drop the new row, re-live the old one.
                let _ = file::restore_replaced(&state.db, old.id, new_id).await;
                let status = match &error {
                    storage::StorageError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                tracing::error!(%error, "replace upload failed");
                error_page(
                    &ctx,
                    status,
                    "The replacement upload failed; the original is unchanged.",
                )
            }
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
    // Scope against the (deleted) row.
    if require_found(
        scoped_file_including_deleted(&state, arr_id, voice_id, file_id).await,
        &ctx,
        Section::Arrangements,
        "File not found.",
    )
    .is_err()
    {
        return error_page(&ctx, StatusCode::NOT_FOUND, "File not found.");
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
