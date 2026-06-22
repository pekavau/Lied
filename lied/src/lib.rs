#![forbid(unsafe_code)]

//! Lied — sheet music management tool.
//!
//! This crate is the application core: configuration, error handling,
//! the route trees (`/admin`, `/v1`, WebDAV, infra), and the wiring for
//! Postgres + MinIO (S3-compatible) clients.

pub mod config;
pub mod error;
pub mod routes;
pub mod state;
pub mod telemetry;
