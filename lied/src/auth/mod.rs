//! Authentication: local accounts, web sessions, programmatic bearer
//! tokens, and WebDAV app passwords — the four independent auth paths
//! (CLAUDE.md "Auth & access", phase-1 item 4 / issue #4).

pub mod admin;
pub mod app_password;
pub mod csrf;
pub mod extractors;
pub mod jwt;
pub mod password;
pub mod ratelimit;
pub mod session;
pub mod webdav;
