//! Domain entities and their repository (sqlx) functions.
//!
//! Each submodule owns one entity from the CLAUDE.md data model: the wire
//! (`camelCase`) struct plus the `sqlx::query!`/`query_as!` functions that
//! read/write it. Phase-1 item 2 introduces only `instrument`, the first
//! entity to get a real `/v1` endpoint (`GET /v1/instruments`); later items
//! add the rest.

pub mod instrument;
