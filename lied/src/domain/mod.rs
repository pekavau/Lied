//! Domain entities and their repository (sqlx) functions.
//!
//! Each submodule owns one entity from the CLAUDE.md data model: the wire
//! (`camelCase`) struct plus the `sqlx::query!`/`query_as!` functions that
//! read/write it. Phase-1 item 2 introduced `instrument`; item 4 (auth)
//! adds `user`, `app_password`, and `audit_log`. Later items add the rest.

pub mod app_password;
pub mod audit_log;
pub mod instrument;
pub mod user;
