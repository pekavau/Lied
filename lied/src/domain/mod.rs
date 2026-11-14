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
pub mod bulk_assign;
pub mod collection;
pub mod collection_item;
pub mod coverage;
pub mod file;
pub mod global_annotation;
pub mod instrument;
pub mod membership;
pub mod organization;
pub mod part_assignment;
pub mod tag;
pub mod user;
pub mod voice;
pub mod work;

/// What an update does to one nullable field: leave it, or replace it (with
/// `Set(None)` clearing it). The domain-side form of a PATCH field that
/// tells *omitted* apart from `null`; a request's `Option<Option<T>>`
/// converts straight into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change<T> {
    /// Leave the stored value as it is.
    Keep,
    /// Replace it — `None` clears it.
    Set(Option<T>),
}

impl<T> Change<T> {
    /// The value this change leaves behind, given the current one.
    pub fn apply(self, current: Option<T>) -> Option<T> {
        match self {
            Change::Keep => current,
            Change::Set(value) => value,
        }
    }
}

/// Omitted (`None`) keeps; present (`Some(v)`, where `v` may be `None` for an
/// explicit `null`) sets.
impl<T> From<Option<Option<T>>> for Change<T> {
    fn from(field: Option<Option<T>>) -> Self {
        match field {
            None => Change::Keep,
            Some(value) => Change::Set(value),
        }
    }
}
