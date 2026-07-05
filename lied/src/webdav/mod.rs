//! WebDAV filesystem backend (issue #8).
//!
//! A [`dav_server::fs::DavFileSystem`] implementation over the Lied entity tree
//! (Postgres identity + MinIO bytes), replacing the auth-only stub in
//! [`crate::routes::webdav`]. Structural path parsing lives in [`path`]; the
//! filesystem, per-role visibility, and MinIO/DB wiring build on top of it.
//!
//! See CLAUDE.md "WebDAV layout" for the canonical directory contract this
//! module implements.
//!
//! # Implementation plan (issue #8)
//!
//! Foundation done: [`path::ResolvedPath`] (parser + tests). Remaining, in the
//! order the task list tracks them:
//!
//! 1. **`fs.rs` — `LiedFs: DavFileSystem`.** Built per request carrying the
//!    authenticated user + `AppState`. Required methods: `metadata`, `read_dir`,
//!    `open`; plus `create_dir`/`remove_file` for writes. Pattern (from
//!    `dav_server::memfs`): `async move { … }.boxed()` for `FsFuture`;
//!    `read_dir` returns `Box::pin(futures_util::stream::iter(v).map(Ok))`.
//!    Leaf types implement `DavMetaData` (`len`/`modified`/`is_dir`),
//!    `DavDirEntry` (`name`/`metadata`), `DavFile`
//!    (`read_bytes`/`write_bytes`/`write_buf`/`seek`/`flush`/`metadata`).
//!    Map DB/absent → `FsError::{NotFound,Forbidden,Exists,GeneralFailure}`.
//! 2. **Role + listing queries.** Resolve org/arr/voice slug→id (soft-delete
//!    aware); `read_dir` honors per-role visibility: staff
//!    (owner/archivist/conductor) see all; a musician/guest sees only
//!    arrangements with ≥1 `part_assignment` for them (query the table directly
//!    — no domain module until #10), and never the full score. Reuse
//!    [`crate::domain::file::list`] / voice listing.
//! 3. **GET/PUT ↔ `File` rows.** `open` read → [`crate::storage::get_object`]
//!    streamed through a `DavFile`; write → buffer then
//!    [`crate::storage::upload_streaming`] to [`crate::domain::file::derived_key`]
//!    + [`crate::domain::file::create`]/`replace`. `format`/`mime` via
//!    [`crate::domain::file::format_and_ext_for_mime`] (unknown ext → 415-ish
//!    `Forbidden`). Audit each write.
//! 4. **Personal annotations** (`AnnotationFile`): straight to MinIO, no `File`
//!    row; author-only-writable, org-readable; audit `personal_annotation.<action>`.
//! 5. **User library** (`LibraryEntry`): free-form MinIO objects under
//!    `users/<user>/library/…`, private to that user.
//!
//! ## Router wiring ([`crate::routes::webdav`])
//!
//! `DavHandler::handle(req)` accepts an axum `Request` directly (axum's body
//! satisfies `http_body::Body`) and returns `Response<dav_server::body::Body>`;
//! wrap the body with `axum::body::Body::new(..)` to return an axum `Response`.
//! Build the handler per request:
//! `DavHandler::builder().filesystem(Box::new(LiedFs::new(state, user))).locksystem(Box::new(dav_server::fakels::FakeLs::new())).build_handler()`.
//! Replace the `stub` handlers; keep the existing app-password `route_layer`.

pub mod path;

pub use path::ResolvedPath;
