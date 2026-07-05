//! WebDAV filesystem backend (issue #8).
//!
//! A [`dav_server::fs::DavFileSystem`] implementation over the Lied entity tree
//! (Postgres identity + MinIO bytes), replacing the auth-only stub in
//! [`crate::routes::webdav`]. Structural path parsing lives in [`path`]; the
//! filesystem, per-role visibility, and MinIO/DB wiring build on top of it.
//!
//! See CLAUDE.md "WebDAV layout" for the canonical directory contract this
//! module implements.

pub mod path;

pub use path::ResolvedPath;
