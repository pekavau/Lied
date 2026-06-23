//! Shared offset/limit pagination envelope for `/v1` list endpoints.
//!
//! Per CLAUDE.md's "HTTP & REST API conventions": list endpoints use
//! offset/limit query params and respond with `{ items, total, limit,
//! offset }`. Default `limit` is the configured default page size, max is
//! `LIED_MAX_PAGE_SIZE` (both come from [`crate::config::AppConfig`]).

use serde::{Deserialize, Serialize};

/// Raw query parameters as received from the client, before clamping.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageParams {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

impl PageParams {
    /// Clamp `limit` into `[1, max_page_size]` (defaulting to
    /// `default_page_size` when absent) and default `offset` to 0.
    pub fn resolve(&self, default_page_size: u32, max_page_size: u32) -> (u32, u32) {
        let limit = self
            .limit
            .unwrap_or(default_page_size)
            .clamp(1, max_page_size);
        let offset = self.offset.unwrap_or(0);
        (limit, offset)
    }
}

/// The `{ items, total, limit, offset }` response envelope.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_when_absent() {
        let params = PageParams {
            limit: None,
            offset: None,
        };
        assert_eq!(params.resolve(50, 200), (50, 0));
    }

    #[test]
    fn resolve_clamps_to_max() {
        let params = PageParams {
            limit: Some(10_000),
            offset: Some(5),
        };
        assert_eq!(params.resolve(50, 200), (200, 5));
    }

    #[test]
    fn resolve_clamps_zero_limit_to_one() {
        let params = PageParams {
            limit: Some(0),
            offset: None,
        };
        assert_eq!(params.resolve(50, 200), (1, 0));
    }
}
