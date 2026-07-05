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
//! Foundation done: [`path::ResolvedPath`] (parser + tests) and
//! [`access`] (per-role visibility queries + tests). Remaining, in task order —
//! each bullet is one step:
//!
//! - **`fs.rs` — `LiedFs: DavFileSystem`.** Built per request carrying the authenticated user + `AppState`. Required methods `metadata`/`read_dir`/`open`, plus `create_dir`/`remove_file` for writes. Pattern (from `dav_server::memfs`): `async move { … }.boxed()` for `FsFuture`; `read_dir` returns `Box::pin(futures_util::stream::iter(v).map(Ok))`. Leaf types impl `DavMetaData` (`len`/`modified`/`is_dir`), `DavDirEntry` (`name`/`metadata`), `DavFile` (`read_bytes`/`write_bytes`/`write_buf`/`seek`/`flush`/`metadata`). Map DB/absent → `FsError::{NotFound,Forbidden,Exists,GeneralFailure}`.
//! - **Role + listing queries (step 2, done in [`access`]).** `read_dir` honors per-role visibility via [`access::visible_arrangement_slugs`]; extend with voice/file listing that reuses [`crate::domain::file::list`]. A restricted user never sees the full score.
//! - **GET/PUT ↔ `File` rows (step 3).** `open` read → [`crate::storage::get_object`] streamed through a `DavFile`; write → buffer then [`crate::storage::upload_streaming`] to [`crate::domain::file::derived_key`] + [`crate::domain::file::create`]/`replace`. `format`/`mime` via [`crate::domain::file::format_and_ext_for_mime`] (unknown ext → `Forbidden`). Audit each write.
//! - **Personal annotations (step 4, `AnnotationFile`).** Straight to MinIO, no `File` row; author-only-writable, org-readable; audit `personal_annotation.<action>`.
//! - **User library (step 5, `LibraryEntry`).** Free-form MinIO objects under `users/<user>/library/…`, private to that user.
//!
//! ## Router wiring ([`crate::routes::webdav`])
//!
//! `DavHandler::handle(req)` accepts an axum `Request` directly (axum's body
//! satisfies `http_body::Body`) and returns `Response<dav_server::body::Body>`;
//! wrap the body with `axum::body::Body::new(..)` to return an axum `Response`.
//! Build the handler per request via `DavHandler::builder().filesystem(..).locksystem(Box::new(dav_server::fakels::FakeLs::new())).build_handler()`,
//! replacing the `stub` handlers and keeping the existing app-password `route_layer`.

pub mod access;
pub mod fs;
pub mod path;

pub use fs::LiedFs;
pub use path::ResolvedPath;
