//! `LiedFs` — the [`dav_server::fs::DavFileSystem`] over the Lied entity tree
//! (issue #8). Backed by Postgres (identity + per-role visibility) and MinIO
//! (bytes). Built per request carrying the authenticated app-password user, so
//! visibility and write authorization are resolved against that identity.
//!
//! - Org arrangements tree: score/voice files are `File` rows (WebDAV ↔ REST
//!   agree, since the object key is derived from the same entity tree); writes
//!   are `owner`/`archivist` only; reads are role-filtered.
//! - Personal annotations (`…/voices/<voice>/annotations/<user>/<file>`): MinIO
//!   objects with no `File` row; author-only-writable, org-readable; audited.
//! - User library (`/users/<user>/library/…`): private free-form MinIO objects.

use std::io::SeekFrom;
use std::time::SystemTime;

use bytes::{Buf, Bytes, BytesMut};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::FutureExt;
use uuid::Uuid;

use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::file;
use crate::domain::membership::Role;
use crate::state::AppState;
use crate::storage;
use crate::webdav::access::{self, Visibility};
use crate::webdav::path::ResolvedPath;

/// A `DavFileSystem` scoped to one authenticated WebDAV user.
#[derive(Clone)]
pub struct LiedFs {
    state: AppState,
    user_id: Uuid,
    user_slug: String,
    request_id: Option<Uuid>,
}

impl LiedFs {
    pub fn new(
        state: AppState,
        user_id: Uuid,
        user_slug: String,
        request_id: Option<Uuid>,
    ) -> LiedFs {
        LiedFs {
            state,
            user_id,
            user_slug,
            request_id,
        }
    }

    fn bucket(&self) -> &str {
        &self.state.config.s3_bucket
    }
}

/// Map a database error to a generic WebDAV failure (500). Not-found is handled
/// at the call site via `Option`, so a bubbled `sqlx::Error` is always a real
/// failure — trace it (WebDAV has no Problem Details body to carry detail).
fn db_err(e: sqlx::Error) -> FsError {
    tracing::error!(error = %e, "webdav database error");
    FsError::GeneralFailure
}

/// Stable synthetic mtime for directory nodes (2020-01-01Z). Directories in the
/// entity tree have no meaningful modification time; a fixed value keeps
/// sync-style clients from seeing the whole tree "change" on every PROPFIND.
fn dir_mtime() -> SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800)
}

fn parse_path(path: &DavPath) -> FsResult<ResolvedPath> {
    let s = std::str::from_utf8(path.as_bytes()).map_err(|_| FsError::NotFound)?;
    ResolvedPath::parse(s).ok_or(FsError::NotFound)
}

/// Split a collections-tree item directory segment (`<index>-<arrangement
/// slug>`, e.g. `1-bolero`) into `(index, arrangement_slug)`. Splits on the
/// *first* `-` only, since the index is always a plain non-negative integer
/// with no `-` of its own (this exactly inverts the construction in
/// [`LiedFs::list`]'s `Collection` arm) — the arrangement slug may itself
/// contain further hyphens. Returns `None` if the segment doesn't have the
/// `<digits>-<rest>` shape.
fn parse_item_segment(seg: &str) -> Option<(i32, &str)> {
    let dash = seg.find('-')?;
    let (idx_str, rest) = seg.split_at(dash);
    let index: i32 = idx_str.parse().ok()?;
    let arr_slug = &rest[1..];
    if arr_slug.is_empty() {
        return None;
    }
    Some((index, arr_slug))
}

// ── leaf metadata / dir-entry types ──────────────────────────────────────────

#[derive(Clone, Debug)]
struct LiedMeta {
    is_dir: bool,
    len: u64,
    modified: SystemTime,
}

impl LiedMeta {
    fn dir() -> LiedMeta {
        LiedMeta {
            is_dir: true,
            len: 0,
            modified: dir_mtime(),
        }
    }
    fn file(len: u64, modified: Option<SystemTime>) -> LiedMeta {
        LiedMeta {
            is_dir: false,
            len,
            modified: modified.unwrap_or_else(SystemTime::now),
        }
    }
}

impl DavMetaData for LiedMeta {
    fn len(&self) -> u64 {
        self.len
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
}

struct LiedDirEntry {
    name: Vec<u8>,
    meta: LiedMeta,
}

impl DavDirEntry for LiedDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) }.boxed()
    }
}

fn dir_entry(name: impl Into<Vec<u8>>, meta: LiedMeta) -> Box<dyn DavDirEntry> {
    Box::new(LiedDirEntry {
        name: name.into(),
        meta,
    })
}

// ── resolution helpers ───────────────────────────────────────────────────────

impl LiedFs {
    /// Resolve an arrangement to `(org_id, arr_id, visibility)`, enforcing the
    /// PROPFIND scope rule: a restricted user who holds no assignment on the
    /// arrangement gets `NotFound` (hidden, not 403).
    async fn resolve_arr(&self, org: &str, arr: &str) -> FsResult<(Uuid, Uuid, Visibility)> {
        let (org_id, vis) = access::resolve_org_visibility(&self.state.db, org, self.user_id)
            .await
            .map_err(db_err)?
            .ok_or(FsError::NotFound)?;
        let arr_id = self
            .arr_id_by_slug(org_id, arr)
            .await?
            .ok_or(FsError::NotFound)?;
        if !vis.is_staff()
            && !access::has_assignment_on_arrangement(&self.state.db, arr_id, self.user_id)
                .await
                .map_err(db_err)?
        {
            return Err(FsError::NotFound);
        }
        Ok((org_id, arr_id, vis))
    }

    /// Resolve an arrangement for a *file-writing* op. Only `owner`/`archivist`
    /// may upload/edit arrangement files (CLAUDE.md permission matrix). Note
    /// this is stricter than [`Visibility::Staff`], which also covers
    /// `conductor` — a conductor has read + collections but not file writes, so
    /// visibility (a read concept) must not be reused to authorize writes.
    async fn resolve_arr_for_write(&self, org: &str, arr: &str) -> FsResult<(Uuid, Uuid)> {
        let (org_id, arr_id, _vis) = self.resolve_arr(org, arr).await?;
        let role = access::role_in_org(&self.state.db, org_id, self.user_id)
            .await
            .map_err(db_err)?;
        if !matches!(role, Some(Role::Owner | Role::Archivist)) {
            return Err(FsError::Forbidden);
        }
        Ok((org_id, arr_id))
    }

    /// Resolve a voice to `(org_id, arr_id, voice_id, visibility)`, enforcing
    /// that a restricted user is assigned to *that voice*.
    async fn resolve_voice(
        &self,
        org: &str,
        arr: &str,
        voice: &str,
    ) -> FsResult<(Uuid, Uuid, Uuid, Visibility)> {
        let (org_id, arr_id, vis) = self.resolve_arr(org, arr).await?;
        let voice_id = self
            .voice_id_by_slug(arr_id, voice)
            .await?
            .ok_or(FsError::NotFound)?;
        if !vis.is_staff()
            && !access::has_assignment_on_voice(&self.state.db, voice_id, self.user_id)
                .await
                .map_err(db_err)?
        {
            return Err(FsError::NotFound);
        }
        Ok((org_id, arr_id, voice_id, vis))
    }

    async fn arr_id_by_slug(&self, org_id: Uuid, slug: &str) -> FsResult<Option<Uuid>> {
        let row = sqlx::query_scalar!(
            r#"SELECT id FROM arrangement
               WHERE organization_id = $1 AND slug = $2 AND deleted_at IS NULL"#,
            org_id,
            slug,
        )
        .fetch_optional(&self.state.db)
        .await
        .map_err(db_err)?;
        Ok(row)
    }

    async fn voice_id_by_slug(&self, arr_id: Uuid, slug: &str) -> FsResult<Option<Uuid>> {
        let row = sqlx::query_scalar!(
            r#"SELECT id FROM voice
               WHERE arrangement_id = $1 AND slug = $2 AND deleted_at IS NULL"#,
            arr_id,
            slug,
        )
        .fetch_optional(&self.state.db)
        .await
        .map_err(db_err)?;
        Ok(row)
    }

    /// Voice `(id, slug)` pairs a user may see under an arrangement: staff see
    /// all live voices; a restricted user sees only assigned ones.
    async fn visible_voices(&self, arr_id: Uuid, vis: Visibility) -> FsResult<Vec<(Uuid, String)>> {
        let rows = if vis.is_staff() {
            sqlx::query!(
                r#"SELECT id, slug FROM voice
                   WHERE arrangement_id = $1 AND deleted_at IS NULL ORDER BY slug"#,
                arr_id,
            )
            .fetch_all(&self.state.db)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|r| (r.id, r.slug))
            .collect()
        } else {
            sqlx::query!(
                r#"SELECT DISTINCT v.id, v.slug FROM voice v
                   JOIN part_assignment pa ON pa.voice_id = v.id
                   JOIN collection_item ci ON ci.id = pa.collection_item_id
                   JOIN collection c ON c.id = ci.collection_id
                   WHERE v.arrangement_id = $1 AND v.deleted_at IS NULL AND pa.user_id = $2
                     AND ci.deleted_at IS NULL AND c.deleted_at IS NULL
                   ORDER BY v.slug"#,
                arr_id,
                self.user_id,
            )
            .fetch_all(&self.state.db)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|r| (r.id, r.slug))
            .collect()
        };
        Ok(rows)
    }

    // ── collections tree (read-only computed view, issue #10) ──────────────

    async fn collection_id_by_slug(&self, org_id: Uuid, slug: &str) -> FsResult<Option<Uuid>> {
        let row = sqlx::query_scalar!(
            r#"SELECT id FROM collection
               WHERE organization_id = $1 AND slug = $2 AND deleted_at IS NULL"#,
            org_id,
            slug,
        )
        .fetch_optional(&self.state.db)
        .await
        .map_err(db_err)?;
        Ok(row)
    }

    /// Resolve a collection to `(org_id, collection_id, visibility)`,
    /// enforcing the collections-subtree scope rule: a restricted user who
    /// holds no assignment anywhere in the collection gets `NotFound`
    /// (hidden, not 403) — mirrors [`Self::resolve_arr`].
    async fn resolve_collection(
        &self,
        org: &str,
        coll: &str,
    ) -> FsResult<(Uuid, Uuid, Visibility)> {
        let (org_id, vis) = access::resolve_org_visibility(&self.state.db, org, self.user_id)
            .await
            .map_err(db_err)?
            .ok_or(FsError::NotFound)?;
        let coll_id = self
            .collection_id_by_slug(org_id, coll)
            .await?
            .ok_or(FsError::NotFound)?;
        if !vis.is_staff()
            && !access::has_assignment_in_collection(&self.state.db, coll_id, self.user_id)
                .await
                .map_err(db_err)?
        {
            return Err(FsError::NotFound);
        }
        Ok((org_id, coll_id, vis))
    }

    /// Resolve a `<index>-<arr-slug>` item segment to
    /// `(org_id, collection_id, item_id, arrangement_id, arrangement_slug,
    /// visibility)`. Enforces that a restricted user holds >=1 assignment on
    /// this specific item (stricter than the collection-level check).
    async fn resolve_collection_item(
        &self,
        org: &str,
        coll: &str,
        item_seg: &str,
    ) -> FsResult<(Uuid, Uuid, Uuid, Uuid, String, Visibility)> {
        let (org_id, coll_id, vis) = self.resolve_collection(org, coll).await?;
        let (index, arr_slug) = parse_item_segment(item_seg).ok_or(FsError::NotFound)?;

        let row = sqlx::query!(
            r#"SELECT ci.id as item_id, ci.arrangement_id, a.slug as arr_slug
               FROM collection_item ci
               JOIN arrangement a ON a.id = ci.arrangement_id
               WHERE ci.collection_id = $1 AND ci.index = $2
                 AND ci.deleted_at IS NULL AND a.deleted_at IS NULL"#,
            coll_id,
            index,
        )
        .fetch_optional(&self.state.db)
        .await
        .map_err(db_err)?;
        let Some(row) = row else {
            return Err(FsError::NotFound);
        };
        // The index resolved to a real item, but its arrangement slug doesn't
        // match the path segment (stale client cache, or a tampered path) —
        // treat as not found rather than silently serving the wrong item.
        if row.arr_slug != arr_slug {
            return Err(FsError::NotFound);
        }

        if !vis.is_staff()
            && !access::has_assignment_on_item(&self.state.db, row.item_id, self.user_id)
                .await
                .map_err(db_err)?
        {
            return Err(FsError::NotFound);
        }

        Ok((
            org_id,
            coll_id,
            row.item_id,
            row.arrangement_id,
            row.arr_slug,
            vis,
        ))
    }

    /// Resolve a voice within a collection item to `(org_id, collection_id,
    /// item_id, arrangement_id, arrangement_slug, voice_id)`. Enforces that a
    /// restricted user is assigned to *this voice on this item specifically*
    /// (CLAUDE.md: stricter than the arrangements-tree assignment check,
    /// which matches the voice on any item).
    async fn resolve_collection_voice(
        &self,
        org: &str,
        coll: &str,
        item_seg: &str,
        voice: &str,
    ) -> FsResult<(Uuid, Uuid, Uuid, Uuid, String, Uuid)> {
        let (org_id, coll_id, item_id, arr_id, arr_slug, vis) =
            self.resolve_collection_item(org, coll, item_seg).await?;
        let voice_id = self
            .voice_id_by_slug(arr_id, voice)
            .await?
            .ok_or(FsError::NotFound)?;
        if !vis.is_staff()
            && !access::has_assignment_on_item_voice(
                &self.state.db,
                item_id,
                voice_id,
                self.user_id,
            )
            .await
            .map_err(db_err)?
        {
            return Err(FsError::NotFound);
        }
        Ok((org_id, coll_id, item_id, arr_id, arr_slug, voice_id))
    }

    /// Voice `(id, slug)` pairs a user may see under a specific collection
    /// item: staff see every live voice of the item's arrangement; a
    /// restricted user sees only voices they are assigned to **on this
    /// item** (not on the arrangement generally — see
    /// [`Self::resolve_collection_voice`]).
    async fn visible_item_voices(
        &self,
        item_id: Uuid,
        arr_id: Uuid,
        vis: Visibility,
    ) -> FsResult<Vec<(Uuid, String)>> {
        let rows = if vis.is_staff() {
            sqlx::query!(
                r#"SELECT id, slug FROM voice
                   WHERE arrangement_id = $1 AND deleted_at IS NULL ORDER BY slug"#,
                arr_id,
            )
            .fetch_all(&self.state.db)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|r| (r.id, r.slug))
            .collect()
        } else {
            sqlx::query!(
                r#"SELECT DISTINCT v.id, v.slug FROM voice v
                   JOIN part_assignment pa ON pa.voice_id = v.id
                   WHERE v.arrangement_id = $1 AND v.deleted_at IS NULL
                     AND pa.collection_item_id = $2 AND pa.user_id = $3
                   ORDER BY v.slug"#,
                arr_id,
                item_id,
                self.user_id,
            )
            .fetch_all(&self.state.db)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|r| (r.id, r.slug))
            .collect()
        };
        Ok(rows)
    }

    /// The WebDAV filename + size + mtime for each live file under
    /// `(arr, voice)` (`voice = None` → full-score files).
    async fn file_entries(
        &self,
        org_slug: &str,
        arr_slug: &str,
        voice_slug: Option<&str>,
        arr_id: Uuid,
        voice_id: Option<Uuid>,
    ) -> FsResult<Vec<LiedDirEntry>> {
        let (files, _total) = file::list(&self.state.db, arr_id, voice_id, 10_000, 0)
            .await
            .map_err(db_err)?;
        let mut entries = Vec::with_capacity(files.len());
        for f in files {
            let Some(filename) = file::webdav_filename(&f.name, &f.mime_type) else {
                continue;
            };
            let (_fmt, ext) = file::format_and_ext_for_mime(&f.mime_type).unwrap_or(("", ""));
            let key = file::derived_key(org_slug, arr_slug, voice_slug, &f.name, ext);
            // Best-effort size; a listing shouldn't fail if a HEAD hiccups.
            let head = storage::head_object(&self.state.s3, self.bucket(), &key)
                .await
                .ok();
            let (len, modified) = head
                .map(|h| (h.size, h.modified))
                .unwrap_or((0, Some(system_time(f.updated_at))));
            entries.push(LiedDirEntry {
                name: filename.into_bytes(),
                meta: LiedMeta::file(len, modified),
            });
        }
        Ok(entries)
    }
}

fn system_time(dt: chrono::DateTime<chrono::Utc>) -> SystemTime {
    let secs = dt.timestamp();
    if secs < 0 {
        SystemTime::now()
    } else {
        std::time::UNIX_EPOCH + std::time::Duration::new(secs as u64, dt.timestamp_subsec_nanos())
    }
}

// ── stat (metadata) ──────────────────────────────────────────────────────────

impl LiedFs {
    async fn stat(&self, rp: &ResolvedPath) -> FsResult<LiedMeta> {
        use ResolvedPath::*;
        match rp {
            Root | OrgsRoot | UsersRoot => Ok(LiedMeta::dir()),

            Org { org } | ArrangementsRoot { org } => {
                access::find_org_id(&self.state.db, org)
                    .await
                    .map_err(db_err)?
                    .ok_or(FsError::NotFound)?;
                Ok(LiedMeta::dir())
            }

            Arrangement { org, arr } | VoicesDir { org, arr } => {
                self.resolve_arr(org, arr).await?;
                Ok(LiedMeta::dir())
            }
            ScoreDir { org, arr } => {
                let (_o, _a, vis) = self.resolve_arr(org, arr).await?;
                // Full score is invisible to restricted users.
                if !vis.is_staff() {
                    return Err(FsError::NotFound);
                }
                Ok(LiedMeta::dir())
            }
            Voice { org, arr, voice } | AnnotationsDir { org, arr, voice } => {
                self.resolve_voice(org, arr, voice).await?;
                Ok(LiedMeta::dir())
            }
            AnnotationsUserDir {
                org, arr, voice, ..
            } => {
                self.resolve_voice(org, arr, voice).await?;
                Ok(LiedMeta::dir())
            }

            ScoreFile { org, arr, file } => {
                let (_o, arr_id, vis) = self.resolve_arr(org, arr).await?;
                if !vis.is_staff() {
                    return Err(FsError::NotFound);
                }
                self.stat_org_file(arr_id, None, file).await
            }
            VoiceFile {
                org,
                arr,
                voice,
                file,
            } => {
                let (_o, arr_id, voice_id, _vis) = self.resolve_voice(org, arr, voice).await?;
                self.stat_org_file(arr_id, Some(voice_id), file).await
            }

            AnnotationFile {
                org,
                arr,
                voice,
                user,
                file,
            } => {
                self.resolve_voice(org, arr, voice).await?;
                let key = annotation_key(org, arr, voice, user, file);
                self.stat_object(&key).await
            }

            CollectionsRoot { org } => {
                access::find_org_id(&self.state.db, org)
                    .await
                    .map_err(db_err)?
                    .ok_or(FsError::NotFound)?;
                Ok(LiedMeta::dir())
            }
            Collection { org, coll } => {
                self.resolve_collection(org, coll).await?;
                Ok(LiedMeta::dir())
            }
            CollectionItemDir { org, coll, item } => {
                self.resolve_collection_item(org, coll, item).await?;
                Ok(LiedMeta::dir())
            }
            CollectionScoreDir { org, coll, item } => {
                let (.., vis) = self.resolve_collection_item(org, coll, item).await?;
                if !vis.is_staff() {
                    return Err(FsError::NotFound);
                }
                Ok(LiedMeta::dir())
            }
            CollectionVoicesDir { org, coll, item } => {
                self.resolve_collection_item(org, coll, item).await?;
                Ok(LiedMeta::dir())
            }
            CollectionVoice {
                org,
                coll,
                item,
                voice,
            } => {
                self.resolve_collection_voice(org, coll, item, voice)
                    .await?;
                Ok(LiedMeta::dir())
            }
            CollectionScoreFile {
                org,
                coll,
                item,
                file,
            } => {
                let (_o, _c, _i, arr_id, _arr_slug, vis) =
                    self.resolve_collection_item(org, coll, item).await?;
                if !vis.is_staff() {
                    return Err(FsError::NotFound);
                }
                self.stat_org_file(arr_id, None, file).await
            }
            CollectionVoiceFile {
                org,
                coll,
                item,
                voice,
                file,
            } => {
                let (_o, _c, _i, arr_id, _arr_slug, voice_id) = self
                    .resolve_collection_voice(org, coll, item, voice)
                    .await?;
                self.stat_org_file(arr_id, Some(voice_id), file).await
            }

            UserHome { user } | LibraryRoot { user } => {
                self.require_own_library(user)?;
                Ok(LiedMeta::dir())
            }
            LibraryEntry { user, rel } => {
                self.require_own_library(user)?;
                let key = library_key(user, rel);
                // A library path is a file if an object exists at the key.
                if let Ok(head) = storage::head_object(&self.state.s3, self.bucket(), &key).await {
                    return Ok(LiedMeta::file(head.size, head.modified));
                }
                // Otherwise it is a directory if it has a directory marker
                // (an empty dir freshly created by MKCOL — the `key/` object
                // itself, which `list_objects` filters out of a listing) or if
                // anything lives under `key/`.
                let marker = format!("{key}/");
                if storage::head_object(&self.state.s3, self.bucket(), &marker)
                    .await
                    .is_ok()
                {
                    return Ok(LiedMeta::dir());
                }
                let listing = storage::list_objects(&self.state.s3, self.bucket(), &marker)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                if listing.objects.is_empty() && listing.dirs.is_empty() {
                    Err(FsError::NotFound)
                } else {
                    Ok(LiedMeta::dir())
                }
            }
        }
    }

    async fn stat_org_file(
        &self,
        arr_id: Uuid,
        voice_id: Option<Uuid>,
        filename: &str,
    ) -> FsResult<LiedMeta> {
        let (stem, ext) = file::split_filename(filename).ok_or(FsError::NotFound)?;
        let mime = file::mime_for_ext(ext).ok_or(FsError::NotFound)?;
        let (format, _ext) = file::format_and_ext_for_mime(mime).ok_or(FsError::NotFound)?;
        let row = file::find_by_location(&self.state.db, arr_id, voice_id, stem, format)
            .await
            .map_err(db_err)?
            .ok_or(FsError::NotFound)?;
        self.stat_object_for_row(&row).await
    }

    async fn stat_object_for_row(&self, row: &file::File) -> FsResult<LiedMeta> {
        // Recompute the key from the entity tree (source of truth).
        let slugs = file::resolve_path_slugs(&self.state.db, row.arrangement_id, row.voice_id)
            .await
            .map_err(db_err)?
            .ok_or(FsError::NotFound)?;
        let (_fmt, ext) = file::format_and_ext_for_mime(&row.mime_type).ok_or(FsError::NotFound)?;
        let key = file::derived_key(
            &slugs.org_slug,
            &slugs.arrangement_slug,
            slugs.voice_slug.as_deref(),
            &row.name,
            ext,
        );
        match storage::head_object(&self.state.s3, self.bucket(), &key).await {
            Ok(h) => Ok(LiedMeta::file(h.size, h.modified)),
            Err(storage::StorageError::NotFound) => {
                Ok(LiedMeta::file(0, Some(system_time(row.updated_at))))
            }
            Err(_) => Err(FsError::GeneralFailure),
        }
    }

    async fn stat_object(&self, key: &str) -> FsResult<LiedMeta> {
        match storage::head_object(&self.state.s3, self.bucket(), key).await {
            Ok(h) => Ok(LiedMeta::file(h.size, h.modified)),
            Err(storage::StorageError::NotFound) => Err(FsError::NotFound),
            Err(_) => Err(FsError::GeneralFailure),
        }
    }

    /// The private-library guard: only the owner may touch their own library.
    fn require_own_library(&self, user: &str) -> FsResult<()> {
        if user == self.user_slug {
            Ok(())
        } else {
            // Hide other users' libraries entirely rather than 403.
            Err(FsError::NotFound)
        }
    }
}

// ── key builders ─────────────────────────────────────────────────────────────

fn annotation_key(org: &str, arr: &str, voice: &str, user: &str, file: &str) -> String {
    format!("orgs/{org}/arrangements/{arr}/voices/{voice}/annotations/{user}/{file}")
}

fn library_key(user: &str, rel: &[String]) -> String {
    format!("users/{user}/library/{}", rel.join("/"))
}

/// Last path segment of an S3 key/prefix (prefixes end in `/`).
fn basename(key: &str) -> String {
    key.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

// ── directory listing (read_dir) ─────────────────────────────────────────────

impl LiedFs {
    async fn list(&self, rp: &ResolvedPath) -> FsResult<Vec<Box<dyn DavDirEntry>>> {
        use ResolvedPath::*;
        let mut out: Vec<Box<dyn DavDirEntry>> = Vec::new();
        match rp {
            Root => {
                out.push(dir_entry("orgs", LiedMeta::dir()));
                out.push(dir_entry("users", LiedMeta::dir()));
            }
            OrgsRoot => {
                for slug in access::visible_org_slugs(&self.state.db, self.user_id)
                    .await
                    .map_err(db_err)?
                {
                    out.push(dir_entry(slug.into_bytes(), LiedMeta::dir()));
                }
            }
            Org { org } => {
                access::find_org_id(&self.state.db, org)
                    .await
                    .map_err(db_err)?
                    .ok_or(FsError::NotFound)?;
                out.push(dir_entry("arrangements", LiedMeta::dir()));
                out.push(dir_entry("collections", LiedMeta::dir()));
            }
            ArrangementsRoot { org } => {
                let (org_id, vis) =
                    access::resolve_org_visibility(&self.state.db, org, self.user_id)
                        .await
                        .map_err(db_err)?
                        .ok_or(FsError::NotFound)?;
                for slug in
                    access::visible_arrangement_slugs(&self.state.db, org_id, self.user_id, vis)
                        .await
                        .map_err(db_err)?
                {
                    out.push(dir_entry(slug.into_bytes(), LiedMeta::dir()));
                }
            }
            Arrangement { org, arr } => {
                let (_o, _a, vis) = self.resolve_arr(org, arr).await?;
                if vis.is_staff() {
                    out.push(dir_entry("score", LiedMeta::dir()));
                }
                out.push(dir_entry("voices", LiedMeta::dir()));
            }
            ScoreDir { org, arr } => {
                let (_o, arr_id, vis) = self.resolve_arr(org, arr).await?;
                if !vis.is_staff() {
                    return Err(FsError::NotFound);
                }
                for e in self.file_entries(org, arr, None, arr_id, None).await? {
                    out.push(Box::new(e));
                }
            }
            VoicesDir { org, arr } => {
                let (_o, arr_id, vis) = self.resolve_arr(org, arr).await?;
                for (_id, slug) in self.visible_voices(arr_id, vis).await? {
                    out.push(dir_entry(slug.into_bytes(), LiedMeta::dir()));
                }
            }
            Voice { org, arr, voice } => {
                let (_o, arr_id, voice_id, _vis) = self.resolve_voice(org, arr, voice).await?;
                for e in self
                    .file_entries(org, arr, Some(voice), arr_id, Some(voice_id))
                    .await?
                {
                    out.push(Box::new(e));
                }
                out.push(dir_entry("annotations", LiedMeta::dir()));
            }
            AnnotationsDir { org, arr, voice } => {
                self.resolve_voice(org, arr, voice).await?;
                let prefix = format!("orgs/{org}/arrangements/{arr}/voices/{voice}/annotations/");
                let listing = storage::list_objects(&self.state.s3, self.bucket(), &prefix)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                for d in listing.dirs {
                    out.push(dir_entry(basename(&d).into_bytes(), LiedMeta::dir()));
                }
            }
            AnnotationsUserDir {
                org,
                arr,
                voice,
                user,
            } => {
                self.resolve_voice(org, arr, voice).await?;
                let prefix =
                    format!("orgs/{org}/arrangements/{arr}/voices/{voice}/annotations/{user}/");
                let listing = storage::list_objects(&self.state.s3, self.bucket(), &prefix)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                for o in listing.objects {
                    out.push(dir_entry(
                        basename(&o.key).into_bytes(),
                        LiedMeta::file(o.size, o.modified),
                    ));
                }
            }
            CollectionsRoot { org } => {
                let (org_id, vis) =
                    access::resolve_org_visibility(&self.state.db, org, self.user_id)
                        .await
                        .map_err(db_err)?
                        .ok_or(FsError::NotFound)?;
                for slug in
                    access::visible_collection_slugs(&self.state.db, org_id, self.user_id, vis)
                        .await
                        .map_err(db_err)?
                {
                    out.push(dir_entry(slug.into_bytes(), LiedMeta::dir()));
                }
            }
            Collection { org, coll } => {
                let (_o, coll_id, vis) = self.resolve_collection(org, coll).await?;
                for item in
                    access::visible_collection_items(&self.state.db, coll_id, self.user_id, vis)
                        .await
                        .map_err(db_err)?
                {
                    out.push(dir_entry(
                        format!("{}-{}", item.index, item.arrangement_slug).into_bytes(),
                        LiedMeta::dir(),
                    ));
                }
            }
            CollectionItemDir { org, coll, item } => {
                let (.., vis) = self.resolve_collection_item(org, coll, item).await?;
                if vis.is_staff() {
                    out.push(dir_entry("score", LiedMeta::dir()));
                }
                out.push(dir_entry("voices", LiedMeta::dir()));
            }
            CollectionScoreDir { org, coll, item } => {
                let (_o, _c, _i, arr_id, arr_slug, vis) =
                    self.resolve_collection_item(org, coll, item).await?;
                if !vis.is_staff() {
                    return Err(FsError::NotFound);
                }
                for e in self
                    .file_entries(org, &arr_slug, None, arr_id, None)
                    .await?
                {
                    out.push(Box::new(e));
                }
            }
            CollectionVoicesDir { org, coll, item } => {
                let (_o, _c, item_id, arr_id, _arr_slug, vis) =
                    self.resolve_collection_item(org, coll, item).await?;
                for (_id, slug) in self.visible_item_voices(item_id, arr_id, vis).await? {
                    out.push(dir_entry(slug.into_bytes(), LiedMeta::dir()));
                }
            }
            CollectionVoice {
                org,
                coll,
                item,
                voice,
            } => {
                let (_o, _c, _i, arr_id, arr_slug, voice_id) = self
                    .resolve_collection_voice(org, coll, item, voice)
                    .await?;
                for e in self
                    .file_entries(org, &arr_slug, Some(voice), arr_id, Some(voice_id))
                    .await?
                {
                    out.push(Box::new(e));
                }
            }
            UsersRoot => {
                out.push(dir_entry(
                    self.user_slug.clone().into_bytes(),
                    LiedMeta::dir(),
                ));
            }
            UserHome { user } => {
                self.require_own_library(user)?;
                out.push(dir_entry("library", LiedMeta::dir()));
            }
            LibraryRoot { user } => {
                self.require_own_library(user)?;
                self.list_library(&format!("users/{user}/library/"), &mut out)
                    .await?;
            }
            LibraryEntry { user, rel } => {
                self.require_own_library(user)?;
                self.list_library(&format!("{}/", library_key(user, rel)), &mut out)
                    .await?;
            }
            ScoreFile { .. }
            | VoiceFile { .. }
            | AnnotationFile { .. }
            | CollectionScoreFile { .. }
            | CollectionVoiceFile { .. } => {
                return Err(FsError::Forbidden);
            }
        }
        Ok(out)
    }

    async fn list_library(
        &self,
        prefix: &str,
        out: &mut Vec<Box<dyn DavDirEntry>>,
    ) -> FsResult<()> {
        let listing = storage::list_objects(&self.state.s3, self.bucket(), prefix)
            .await
            .map_err(|_| FsError::GeneralFailure)?;
        for d in listing.dirs {
            out.push(dir_entry(basename(&d).into_bytes(), LiedMeta::dir()));
        }
        for o in listing.objects {
            if o.key.ends_with('/') {
                continue; // directory marker
            }
            out.push(dir_entry(
                basename(&o.key).into_bytes(),
                LiedMeta::file(o.size, o.modified),
            ));
        }
        Ok(())
    }
}

// ── open (read + write) ──────────────────────────────────────────────────────

/// A resolved, authorized write destination, committed on `flush`.
enum WriteTarget {
    /// Placeholder after commit / for non-write files.
    Noop,
    /// A score or voice file → object + `File` row.
    OrgFile {
        org_id: Uuid,
        arr_id: Uuid,
        voice_id: Option<Uuid>,
        name: String,
        format: &'static str,
        mime: &'static str,
        key: String,
    },
    /// A personal annotation → object only, audited.
    Annotation {
        org_id: Uuid,
        voice_id: Uuid,
        user_slug: String,
        name: String,
        key: String,
        mime: String,
    },
    /// A private-library file → object only.
    Library { key: String, mime: String },
}

/// An open WebDAV file: an in-memory read buffer, or a write buffer committed
/// on flush.
// The `Write` variant is larger than `Read` (it carries the `LiedFs`/`AppState`
// needed to commit), but a `LiedFile` is always returned boxed as
// `Box<dyn DavFile>`, so the on-stack size difference never materializes.
#[allow(clippy::large_enum_variant)]
enum LiedFile {
    Read {
        data: Bytes,
        pos: usize,
        meta: LiedMeta,
    },
    Write {
        fs: LiedFs,
        target: WriteTarget,
        buf: BytesMut,
        max: u64,
        committed: bool,
    },
}

impl std::fmt::Debug for LiedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiedFile::Read { data, pos, .. } => f
                .debug_struct("LiedFile::Read")
                .field("len", &data.len())
                .field("pos", pos)
                .finish(),
            LiedFile::Write { buf, committed, .. } => f
                .debug_struct("LiedFile::Write")
                .field("buffered", &buf.len())
                .field("committed", committed)
                .finish(),
        }
    }
}

impl LiedFile {
    fn append(&mut self, data: &[u8]) -> FsResult<()> {
        match self {
            LiedFile::Write { buf, max, .. } => {
                if buf.len() as u64 + data.len() as u64 > *max {
                    return Err(FsError::TooLarge);
                }
                buf.extend_from_slice(data);
                Ok(())
            }
            LiedFile::Read { .. } => Err(FsError::Forbidden),
        }
    }
}

fn map_storage_write(e: storage::StorageError) -> FsError {
    match e {
        storage::StorageError::TooLarge { .. } => FsError::TooLarge,
        _ => FsError::GeneralFailure,
    }
}

/// Commit a buffered write: upload the object, then (for org files) create or
/// replace the `File` row, and audit.
async fn commit_write(fs: &LiedFs, target: &WriteTarget, data: Bytes) -> FsResult<()> {
    match target {
        WriteTarget::Noop => Ok(()),
        WriteTarget::OrgFile {
            org_id,
            arr_id,
            voice_id,
            name,
            format,
            mime,
            key,
        } => {
            // DB identity first, object bytes second (mirrors the REST path in
            // routes::files): the safe failure mode is a DB row referencing a
            // not-yet-uploaded object (a detectable 404), never a mutated object
            // with a stale/duplicate row. On upload failure we compensate.
            let existing = file::find_by_location(&fs.state.db, *arr_id, *voice_id, name, format)
                .await
                .map_err(db_err)?;
            let new_id = Uuid::now_v7();
            let new = file::NewFile {
                arrangement_id: *arr_id,
                voice_id: *voice_id,
                name,
                format,
                mime_type: mime,
                derived_from_file_id: None,
                conversion_quality: None,
                created_by: Some(fs.user_id),
            };
            let old_id = existing.as_ref().map(|f| f.id);
            let action = match &existing {
                Some(old) => {
                    file::replace(&fs.state.db, old.id, new_id, new)
                        .await
                        .map_err(|_| FsError::GeneralFailure)?;
                    "file.replace"
                }
                None => {
                    file::create(&fs.state.db, new_id, new)
                        .await
                        .map_err(|_| FsError::GeneralFailure)?;
                    "file.create"
                }
            };

            if let Err(e) = storage::upload_bytes(
                &fs.state.s3,
                fs.bucket(),
                key,
                mime,
                fs.state.config.max_upload_bytes,
                data,
            )
            .await
            {
                // Roll the DB back to the pre-write state so we never leave a
                // live row pointing at a missing object.
                match old_id {
                    Some(old) => {
                        let _ = file::restore_replaced(&fs.state.db, old, new_id).await;
                    }
                    None => {
                        let _ = file::soft_delete(&fs.state.db, new_id).await;
                    }
                }
                return Err(map_storage_write(e));
            }
            audit(
                &fs.state.db,
                &AuditContext {
                    actor_user_id: Some(fs.user_id),
                    org_id: Some(*org_id),
                    request_id: fs.request_id,
                },
                action,
                "file",
                Some(new_id),
                serde_json::json!({ "via": "webdav", "name": name, "format": format }),
            )
            .await;
            Ok(())
        }
        WriteTarget::Annotation {
            org_id,
            voice_id,
            user_slug,
            name,
            key,
            mime,
        } => {
            let existed = storage::head_object(&fs.state.s3, fs.bucket(), key)
                .await
                .is_ok();
            storage::upload_bytes(
                &fs.state.s3,
                fs.bucket(),
                key,
                mime,
                fs.state.config.max_upload_bytes,
                data,
            )
            .await
            .map_err(map_storage_write)?;
            let action = if existed {
                "personal_annotation.replace"
            } else {
                "personal_annotation.create"
            };
            audit(
                &fs.state.db,
                &AuditContext {
                    actor_user_id: Some(fs.user_id),
                    org_id: Some(*org_id),
                    request_id: fs.request_id,
                },
                action,
                "personal_annotation",
                None,
                serde_json::json!({ "voiceId": voice_id, "userSlug": user_slug, "name": name }),
            )
            .await;
            Ok(())
        }
        WriteTarget::Library { key, mime } => {
            // Deliberately unaudited: `/users/<user>/library/` is a private
            // personal area, not an org-scoped persistent entity. The audit log
            // tracks org content and auth events; private-library writes are
            // outside that scope (unlike personal annotations, which live under
            // an org's arrangement tree and are audited).
            storage::upload_bytes(
                &fs.state.s3,
                fs.bucket(),
                key,
                mime,
                fs.state.config.max_upload_bytes,
                data,
            )
            .await
            .map_err(map_storage_write)?;
            Ok(())
        }
    }
}

impl LiedFs {
    async fn open_read(&self, rp: &ResolvedPath) -> FsResult<Box<dyn DavFile>> {
        let (key, meta) = self.file_key_for_read(rp).await?;
        let obj = storage::get_object(&self.state.s3, self.bucket(), &key, None)
            .await
            .map_err(|e| match e {
                storage::StorageError::NotFound => FsError::NotFound,
                _ => FsError::GeneralFailure,
            })?;
        let data = obj
            .body
            .collect()
            .await
            .map(|d| d.into_bytes())
            .map_err(|_| FsError::GeneralFailure)?;
        Ok(Box::new(LiedFile::Read { data, pos: 0, meta }))
    }

    async fn file_key_for_read(&self, rp: &ResolvedPath) -> FsResult<(String, LiedMeta)> {
        use ResolvedPath::*;
        match rp {
            ScoreFile { org, arr, file } => {
                let (_o, arr_id, vis) = self.resolve_arr(org, arr).await?;
                if !vis.is_staff() {
                    return Err(FsError::NotFound);
                }
                self.org_file_key(org, arr, None, arr_id, None, file).await
            }
            VoiceFile {
                org,
                arr,
                voice,
                file,
            } => {
                let (_o, arr_id, voice_id, _vis) = self.resolve_voice(org, arr, voice).await?;
                self.org_file_key(org, arr, Some(voice), arr_id, Some(voice_id), file)
                    .await
            }
            AnnotationFile {
                org,
                arr,
                voice,
                user,
                file,
            } => {
                self.resolve_voice(org, arr, voice).await?; // org-readable
                let key = annotation_key(org, arr, voice, user, file);
                let meta = self.stat_object(&key).await?;
                Ok((key, meta))
            }
            LibraryEntry { user, rel } => {
                self.require_own_library(user)?;
                let key = library_key(user, rel);
                let meta = self.stat_object(&key).await?;
                Ok((key, meta))
            }
            CollectionScoreFile {
                org,
                coll,
                item,
                file,
            } => {
                let (_o, _c, _i, arr_id, arr_slug, vis) =
                    self.resolve_collection_item(org, coll, item).await?;
                if !vis.is_staff() {
                    return Err(FsError::NotFound);
                }
                self.org_file_key(org, &arr_slug, None, arr_id, None, file)
                    .await
            }
            CollectionVoiceFile {
                org,
                coll,
                item,
                voice,
                file,
            } => {
                let (_o, _c, _i, arr_id, arr_slug, voice_id) = self
                    .resolve_collection_voice(org, coll, item, voice)
                    .await?;
                self.org_file_key(org, &arr_slug, Some(voice), arr_id, Some(voice_id), file)
                    .await
            }
            _ => Err(FsError::Forbidden),
        }
    }

    async fn org_file_key(
        &self,
        org: &str,
        arr: &str,
        voice_slug: Option<&str>,
        arr_id: Uuid,
        voice_id: Option<Uuid>,
        filename: &str,
    ) -> FsResult<(String, LiedMeta)> {
        let (stem, ext) = file::split_filename(filename).ok_or(FsError::NotFound)?;
        let mime = file::mime_for_ext(ext).ok_or(FsError::NotFound)?;
        let (format, _e) = file::format_and_ext_for_mime(mime).ok_or(FsError::NotFound)?;
        let row = file::find_by_location(&self.state.db, arr_id, voice_id, stem, format)
            .await
            .map_err(db_err)?
            .ok_or(FsError::NotFound)?;
        let (_f, cext) = file::format_and_ext_for_mime(&row.mime_type).ok_or(FsError::NotFound)?;
        let key = file::derived_key(org, arr, voice_slug, &row.name, cext);
        let meta = self.stat_object_for_row(&row).await?;
        Ok((key, meta))
    }

    async fn open_write(&self, rp: &ResolvedPath) -> FsResult<Box<dyn DavFile>> {
        let target = self.resolve_write_target(rp).await?;
        Ok(Box::new(LiedFile::Write {
            fs: self.clone(),
            target,
            buf: BytesMut::new(),
            max: self.state.config.max_upload_bytes,
            committed: false,
        }))
    }

    async fn resolve_write_target(&self, rp: &ResolvedPath) -> FsResult<WriteTarget> {
        use ResolvedPath::*;
        match rp {
            ScoreFile { org, arr, file } => {
                let (org_id, arr_id) = self.resolve_arr_for_write(org, arr).await?;
                self.org_write_target(org_id, org, arr, None, arr_id, None, file)
            }
            VoiceFile {
                org,
                arr,
                voice,
                file,
            } => {
                let (org_id, arr_id) = self.resolve_arr_for_write(org, arr).await?;
                let voice_id = self
                    .voice_id_by_slug(arr_id, voice)
                    .await?
                    .ok_or(FsError::NotFound)?;
                self.org_write_target(org_id, org, arr, Some(voice), arr_id, Some(voice_id), file)
            }
            AnnotationFile {
                org,
                arr,
                voice,
                user,
                file,
            } => {
                // Author-only-writable.
                if user != &self.user_slug {
                    return Err(FsError::Forbidden);
                }
                let (org_id, _arr_id, voice_id, _vis) = self.resolve_voice(org, arr, voice).await?;
                let key = annotation_key(org, arr, voice, user, file);
                let ext = file::split_filename(file).map(|(_, e)| e).unwrap_or("");
                let mime = file::mime_for_ext(ext)
                    .unwrap_or("application/octet-stream")
                    .to_string();
                Ok(WriteTarget::Annotation {
                    org_id,
                    voice_id,
                    user_slug: user.clone(),
                    name: file.clone(),
                    key,
                    mime,
                })
            }
            LibraryEntry { user, rel } => {
                self.require_own_library(user)?;
                let key = library_key(user, rel);
                let ext = rel
                    .last()
                    .and_then(|f| file::split_filename(f))
                    .map(|(_, e)| e)
                    .unwrap_or("");
                let mime = file::mime_for_ext(ext)
                    .unwrap_or("application/octet-stream")
                    .to_string();
                Ok(WriteTarget::Library { key, mime })
            }
            _ => Err(FsError::Forbidden),
        }
    }

    // All seven inputs are genuinely needed to build the object key + File row;
    // bundling them would only obscure the call sites.
    #[allow(clippy::too_many_arguments)]
    fn org_write_target(
        &self,
        org_id: Uuid,
        org: &str,
        arr: &str,
        voice_slug: Option<&str>,
        arr_id: Uuid,
        voice_id: Option<Uuid>,
        filename: &str,
    ) -> FsResult<WriteTarget> {
        let (stem, ext) = file::split_filename(filename).ok_or(FsError::Forbidden)?;
        // Unsupported extension → 403 (the WebDAV analogue of 415).
        let mime = file::mime_for_ext(ext).ok_or(FsError::Forbidden)?;
        let (format, cext) = file::format_and_ext_for_mime(mime).ok_or(FsError::Forbidden)?;
        let key = file::derived_key(org, arr, voice_slug, stem, cext);
        Ok(WriteTarget::OrgFile {
            org_id,
            arr_id,
            voice_id,
            name: stem.to_string(),
            format,
            mime,
            key,
        })
    }

    async fn remove(&self, rp: &ResolvedPath) -> FsResult<()> {
        use ResolvedPath::*;
        match rp {
            ScoreFile { org, arr, file } => {
                let (org_id, arr_id) = self.resolve_arr_for_write(org, arr).await?;
                self.remove_org_file(org_id, arr_id, None, file).await
            }
            VoiceFile {
                org,
                arr,
                voice,
                file,
            } => {
                let (org_id, arr_id) = self.resolve_arr_for_write(org, arr).await?;
                let voice_id = self
                    .voice_id_by_slug(arr_id, voice)
                    .await?
                    .ok_or(FsError::NotFound)?;
                self.remove_org_file(org_id, arr_id, Some(voice_id), file)
                    .await
            }
            AnnotationFile {
                org,
                arr,
                voice,
                user,
                file,
            } => {
                if user != &self.user_slug {
                    return Err(FsError::Forbidden);
                }
                let (org_id, _a, voice_id, _v) = self.resolve_voice(org, arr, voice).await?;
                let key = annotation_key(org, arr, voice, user, file);
                storage::delete_object(&self.state.s3, self.bucket(), &key)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                audit(
                    &self.state.db,
                    &AuditContext {
                        actor_user_id: Some(self.user_id),
                        org_id: Some(org_id),
                        request_id: self.request_id,
                    },
                    "personal_annotation.delete",
                    "personal_annotation",
                    None,
                    serde_json::json!({ "voiceId": voice_id, "userSlug": user, "name": file }),
                )
                .await;
                Ok(())
            }
            LibraryEntry { user, rel } => {
                self.require_own_library(user)?;
                let key = library_key(user, rel);
                storage::delete_object(&self.state.s3, self.bucket(), &key)
                    .await
                    .map_err(|_| FsError::GeneralFailure)?;
                Ok(())
            }
            _ => Err(FsError::Forbidden),
        }
    }

    async fn remove_org_file(
        &self,
        org_id: Uuid,
        arr_id: Uuid,
        voice_id: Option<Uuid>,
        filename: &str,
    ) -> FsResult<()> {
        let (stem, ext) = file::split_filename(filename).ok_or(FsError::NotFound)?;
        let mime = file::mime_for_ext(ext).ok_or(FsError::NotFound)?;
        let (format, _e) = file::format_and_ext_for_mime(mime).ok_or(FsError::NotFound)?;
        let row = file::find_by_location(&self.state.db, arr_id, voice_id, stem, format)
            .await
            .map_err(db_err)?
            .ok_or(FsError::NotFound)?;
        // Soft-delete (MinIO object retained per CLAUDE.md).
        file::soft_delete(&self.state.db, row.id)
            .await
            .map_err(db_err)?;
        audit(
            &self.state.db,
            &AuditContext {
                actor_user_id: Some(self.user_id),
                org_id: Some(org_id),
                request_id: self.request_id,
            },
            "file.delete",
            "file",
            Some(row.id),
            serde_json::json!({ "via": "webdav" }),
        )
        .await;
        Ok(())
    }
}

// ── DavFile ──────────────────────────────────────────────────────────────────

impl DavFile for LiedFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = match self {
            LiedFile::Read { meta, .. } => meta.clone(),
            LiedFile::Write { buf, .. } => LiedMeta::file(buf.len() as u64, None),
        };
        async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) }.boxed()
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        let res = match self {
            LiedFile::Read { data, pos, .. } => {
                let start = (*pos).min(data.len());
                let end = start.saturating_add(count).min(data.len());
                let chunk = data.slice(start..end);
                *pos = end;
                Ok(chunk)
            }
            LiedFile::Write { .. } => Err(FsError::Forbidden),
        };
        async move { res }.boxed()
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        let res = self.append(&buf);
        async move { res }.boxed()
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        let mut collected = BytesMut::with_capacity(buf.remaining());
        while buf.has_remaining() {
            let chunk = buf.chunk();
            collected.extend_from_slice(chunk);
            let n = chunk.len();
            buf.advance(n);
        }
        let res = self.append(&collected);
        async move { res }.boxed()
    }

    fn seek(&mut self, whence: SeekFrom) -> FsFuture<'_, u64> {
        let res = match self {
            LiedFile::Read { data, pos, .. } => {
                let len = data.len() as i64;
                let target = match whence {
                    SeekFrom::Start(n) => n as i64,
                    SeekFrom::End(n) => len + n,
                    SeekFrom::Current(n) => *pos as i64 + n,
                };
                if target < 0 {
                    Err(FsError::GeneralFailure)
                } else {
                    *pos = (target as usize).min(data.len());
                    Ok(*pos as u64)
                }
            }
            // Writes are append-only; only a no-op seek to the current end is
            // accepted (some clients issue one before writing).
            LiedFile::Write { buf, .. } => match whence {
                SeekFrom::Start(n) if n == buf.len() as u64 => Ok(n),
                SeekFrom::Current(0) => Ok(buf.len() as u64),
                _ => Err(FsError::GeneralFailure),
            },
        };
        async move { res }.boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        match self {
            LiedFile::Read { .. } => async move { Ok(()) }.boxed(),
            LiedFile::Write {
                fs,
                target,
                buf,
                committed,
                ..
            } => {
                if *committed {
                    return async move { Ok(()) }.boxed();
                }
                *committed = true;
                let fs = fs.clone();
                let target = std::mem::replace(target, WriteTarget::Noop);
                let data = std::mem::take(buf).freeze();
                async move { commit_write(&fs, &target, data).await }.boxed()
            }
        }
    }
}

// ── DavFileSystem ────────────────────────────────────────────────────────────

impl DavFileSystem for LiedFs {
    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let rp = parse_path(path)?;
            let meta = self.stat(&rp).await?;
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let rp = parse_path(path)?;
            let entries = self.list(&rp).await?;
            let stream = futures_util::stream::iter(entries.into_iter().map(Ok));
            Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>)
        }
        .boxed()
    }

    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let rp = parse_path(path)?;
            if options.write || options.append || options.truncate || options.create {
                self.open_write(&rp).await
            } else {
                self.open_read(&rp).await
            }
        }
        .boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rp = parse_path(path)?;
            self.remove(&rp).await
        }
        .boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rp = parse_path(path)?;
            match &rp {
                // Library subdirectories: write a zero-byte directory marker so
                // the PROPFIND listing shows the folder before it has children.
                ResolvedPath::LibraryEntry { user, rel } => {
                    self.require_own_library(user)?;
                    let key = format!("{}/", library_key(user, rel));
                    storage::upload_bytes(
                        &self.state.s3,
                        self.bucket(),
                        &key,
                        "application/x-directory",
                        self.state.config.max_upload_bytes,
                        Bytes::new(),
                    )
                    .await
                    .map_err(map_storage_write)?;
                    Ok(())
                }
                // The annotations author dir is implicit; accept MKCOL as a
                // no-op for the owner (once the voice is confirmed to exist and
                // be visible) so clients can create it before PUT.
                ResolvedPath::AnnotationsUserDir {
                    org,
                    arr,
                    voice,
                    user,
                } if user == &self.user_slug => {
                    self.resolve_voice(org, arr, voice).await?;
                    Ok(())
                }
                _ => Err(FsError::Forbidden),
            }
        }
        .boxed()
    }
}
