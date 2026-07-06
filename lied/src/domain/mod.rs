//! Domain entities and their repository (sqlx) functions.
//!
//! Each submodule owns one entity from the CLAUDE.md data model: the wire
//! (`camelCase`) struct plus the `sqlx::query!`/`query_as!` functions that
//! read/write it. Phase-1 item 2 introduced `instrument`; item 4 (auth)
//! added `user`, `app_password`, and `audit_log`. Item 5 (issue #5) added
//! `organization` and `membership`. Item 6 (issue #6) adds `work`,
//! `arrangement`, `voice`, and `tag` (the latter also owns the
//! `arrangement_tag` join). Later items add the rest.

pub mod app_password;
pub mod arrangement;
pub mod audit_log;
pub mod collection;
pub mod collection_item;
pub mod file;
pub mod instrument;
pub mod membership;
pub mod organization;
pub mod part_assignment;
pub mod tag;
pub mod user;
pub mod voice;
pub mod work;
