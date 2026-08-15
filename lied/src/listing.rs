//! Reusable sort/filter/ETag helpers for `/v1` list and detail endpoints
//! (CLAUDE.md "HTTP & REST API conventions": Sort & filter, Optimistic
//! concurrency).
//!
//! This item (issue #5, Org/User/Membership management) is the first to need
//! these conventions, so the helpers live here rather than ad-hoc per
//! handler — every later phase-1 item reuses them verbatim.
//!
//! Design: each endpoint declares an **allowlist** of sortable/filterable
//! fields mapping a wire field name to a fixed SQL fragment (column
//! reference, already trusted/static). User input is only ever used to
//! *select* among these fixed fragments via a `match`/lookup — never
//! interpolated into SQL directly. An unknown field or direction is a `400`
//! Problem Details ([`AppError::Validation`]) rather than silently ignored,
//! per CLAUDE.md.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use garde::Report;
use serde::Deserialize;

/// Raw `?sort=field:dir` query parameter, before allowlist validation.
#[derive(Debug, Deserialize, Default)]
pub struct SortParam {
    pub sort: Option<String>,
}

/// A resolved sort: the SQL fragment to order by, plus the direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    pub fn as_sql(&self) -> &'static str {
        match self {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        }
    }
}

/// Parse and validate a `sort=field:dir` parameter against a per-endpoint
/// allowlist (`field name -> trusted SQL fragment`). Returns the matched SQL
/// fragment and direction, or a `400` Problem Details on an unknown field or
/// direction. `None`/empty input resolves to `default`.
pub fn resolve_sort<'a>(
    raw: Option<&str>,
    allowlist: &'a [(&'a str, &'a str)],
    default: (&'a str, SortDirection),
) -> Result<(&'a str, SortDirection), Report> {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else {
        return Ok(default);
    };

    let mut report = Report::new();

    let (field, dir) = match raw.split_once(':') {
        Some((field, dir)) => (field, dir),
        None => (raw, "asc"),
    };

    let sql_fragment = allowlist
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, fragment)| *fragment);

    let direction = match dir {
        "asc" => Some(SortDirection::Asc),
        "desc" => Some(SortDirection::Desc),
        _ => None,
    };

    let Some(sql_fragment) = sql_fragment else {
        report.append(
            garde::Path::new("sort"),
            garde::Error::new(format!(
                "unknown sort field '{field}'; allowed: {}",
                allowlist
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        );
        return Err(report);
    };

    let Some(direction) = direction else {
        report.append(
            garde::Path::new("sort"),
            garde::Error::new(format!(
                "unknown sort direction '{dir}'; allowed: asc, desc"
            )),
        );
        return Err(report);
    };

    Ok((sql_fragment, direction))
}

/// Raw `?filter[field]=value` query parameters, before allowlist validation.
/// Axum's `Query` extractor flattens `filter[x]=y` into a map under the
/// `filter` key when the target type is a newtype wrapping a `HashMap`, via
/// serde_qs-style bracket parsing — but axum's built-in `Query` (serde_urlencoded)
/// does NOT support bracket syntax natively. We therefore parse the raw query
/// string ourselves in [`parse_filters`] rather than relying on `Query<T>`.
pub type RawFilters = HashMap<String, String>;

/// Parse `filter[field]=value` pairs out of a raw query string. Multiple
/// filters AND together (collected into the map; the caller ANDs them in
/// SQL). Unrecognized non-`filter[...]` params (e.g. `sort`, `limit`,
/// `offset`, `q`) are ignored here — they're parsed by their own extractors.
pub fn parse_filters(query: &str) -> RawFilters {
    let mut filters = HashMap::new();
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        // Decode BEFORE matching the prefix: a browser submitting a GET form
        // percent-encodes the brackets (`filter%5Bstatus%5D`), and matching the
        // raw key would silently drop the filter — returning everything, which
        // reads as "no results were excluded" rather than "your filter was
        // ignored".
        let decoded_key = percent_decode(key);
        let Some(field) = decoded_key
            .strip_prefix("filter[")
            .and_then(|s| s.strip_suffix(']'))
        else {
            continue;
        };
        let decoded_value = percent_decode(value);
        filters.insert(field.to_string(), decoded_value);
    }
    filters
}

/// Minimal percent-decoding sufficient for query-string values (`+` as
/// space, `%XX` escapes). Avoids pulling in a new dependency for this.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) =
                    u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
                {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Validate raw filters against a per-endpoint allowlist of field names.
/// Returns the validated `(field, value)` pairs in allowlist order (for
/// deterministic SQL fragment ordering), or a `400` Problem Details on any
/// unrecognized field.
pub fn resolve_filters(
    raw: &RawFilters,
    allowlist: &[&str],
) -> Result<Vec<(String, String)>, Report> {
    let mut report = Report::new();
    let mut resolved = Vec::new();

    for (field, value) in raw {
        if allowlist.contains(&field.as_str()) {
            resolved.push((field.clone(), value.clone()));
        } else {
            report.append(
                garde::Path::new("filter"),
                garde::Error::new(format!(
                    "unknown filter field '{field}'; allowed: {}",
                    allowlist.join(", ")
                )),
            );
        }
    }

    if report.is_empty() {
        Ok(resolved)
    } else {
        Err(report)
    }
}

/// Derive the `ETag` header value from an entity's `updated_at`: the
/// millisecond-epoch timestamp, quoted (CLAUDE.md: "ETags derived from
/// `updated_at` (millisecond epoch)").
pub fn etag_for(updated_at: DateTime<Utc>) -> String {
    format!("\"{}\"", updated_at.timestamp_millis())
}

/// Parse an `If-Match` header value back into the millisecond-epoch it
/// encodes, stripping the surrounding quotes a well-formed `ETag` carries.
/// Returns `None` if the header is absent or doesn't parse as an integer —
/// callers treat that as "no valid precondition supplied".
pub fn parse_if_match(header_value: Option<&str>) -> Option<i64> {
    header_value
        .map(|v| v.trim().trim_matches('"'))
        .and_then(|v| v.parse::<i64>().ok())
}

/// Check an `If-Match` header against an entity's current `updated_at`.
/// Returns `Ok(())` when they match (millisecond precision); `Err(())` on
/// any mismatch *or* a missing/unparseable header — callers map `Err` to
/// [`crate::error::AppError::PreconditionFailed`]. Per CLAUDE.md,
/// `PATCH`/`PUT`/`DELETE` MUST send `If-Match`, so "missing" is itself a
/// precondition failure, not a pass-through.
///
/// The `()` error deliberately carries no information: every caller maps it
/// to the exact same `AppError::PreconditionFailed` regardless of *why* the
/// check failed (stale vs. missing vs. unparseable), so a richer error type
/// would only add variants nothing inspects.
#[allow(clippy::result_unit_err)]
pub fn check_if_match(header_value: Option<&str>, updated_at: DateTime<Utc>) -> Result<(), ()> {
    let current_ms = updated_at.timestamp_millis();
    match parse_if_match(header_value) {
        Some(supplied_ms) if supplied_ms == current_ms => Ok(()),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const ALLOWLIST: &[(&str, &str)] = &[("name", "o.name"), ("createdAt", "o.created_at")];

    #[test]
    fn parse_filters_accepts_percent_encoded_brackets() {
        // A browser GET form encodes the brackets; the hand-written curl in a
        // test does not. Both must reach the same filter.
        let literal = parse_filters("filter[status]=active&limit=50");
        let encoded = parse_filters("filter%5Bstatus%5D=active&limit=50");
        assert_eq!(literal.get("status").map(String::as_str), Some("active"));
        assert_eq!(
            encoded.get("status").map(String::as_str),
            Some("active"),
            "an encoded filter key must not be silently dropped"
        );
        assert_eq!(literal, encoded);
    }

    #[test]
    fn parse_filters_decodes_keys_and_values() {
        let filters = parse_filters("filter%5BdurationMaxSeconds%5D=300&filter[tag]=a%2Cb");
        assert_eq!(
            filters.get("durationMaxSeconds").map(String::as_str),
            Some("300")
        );
        assert_eq!(filters.get("tag").map(String::as_str), Some("a,b"));
    }

    #[test]
    fn resolve_sort_defaults_when_absent() {
        let (frag, dir) =
            resolve_sort(None, ALLOWLIST, ("o.name", SortDirection::Asc)).expect("default sort");
        assert_eq!(frag, "o.name");
        assert_eq!(dir, SortDirection::Asc);
    }

    #[test]
    fn resolve_sort_accepts_allowlisted_field() {
        let (frag, dir) = resolve_sort(
            Some("createdAt:desc"),
            ALLOWLIST,
            ("o.name", SortDirection::Asc),
        )
        .expect("valid sort");
        assert_eq!(frag, "o.created_at");
        assert_eq!(dir, SortDirection::Desc);
    }

    #[test]
    fn resolve_sort_defaults_direction_to_asc_when_omitted() {
        let (frag, dir) =
            resolve_sort(Some("name"), ALLOWLIST, ("o.name", SortDirection::Asc)).unwrap();
        assert_eq!(frag, "o.name");
        assert_eq!(dir, SortDirection::Asc);
    }

    #[test]
    fn resolve_sort_rejects_unknown_field() {
        let result = resolve_sort(
            Some("dangerous; DROP TABLE organization--:asc"),
            ALLOWLIST,
            ("o.name", SortDirection::Asc),
        );
        assert!(result.is_err());
    }

    #[test]
    fn resolve_sort_rejects_unknown_direction() {
        let result = resolve_sort(
            Some("name:sideways"),
            ALLOWLIST,
            ("o.name", SortDirection::Asc),
        );
        assert!(result.is_err());
    }

    #[test]
    fn parse_filters_extracts_bracket_syntax() {
        let filters =
            parse_filters("limit=10&filter[role]=owner&filter[name]=foo%20bar&sort=name:asc");
        assert_eq!(filters.get("role"), Some(&"owner".to_string()));
        assert_eq!(filters.get("name"), Some(&"foo bar".to_string()));
        assert_eq!(filters.len(), 2);
    }

    #[test]
    fn parse_filters_decodes_plus_as_space() {
        let filters = parse_filters("filter[name]=foo+bar");
        assert_eq!(filters.get("name"), Some(&"foo bar".to_string()));
    }

    #[test]
    fn resolve_filters_accepts_allowlisted_fields() {
        let mut raw = RawFilters::new();
        raw.insert("role".to_string(), "owner".to_string());
        let resolved = resolve_filters(&raw, &["role"]).expect("valid filter");
        assert_eq!(resolved, vec![("role".to_string(), "owner".to_string())]);
    }

    #[test]
    fn resolve_filters_rejects_unknown_field() {
        let mut raw = RawFilters::new();
        raw.insert("password_hash".to_string(), "x".to_string());
        let result = resolve_filters(&raw, &["role"]);
        assert!(result.is_err());
    }

    #[test]
    fn etag_for_uses_millisecond_epoch() {
        let dt = Utc.timestamp_millis_opt(1_700_000_000_123).unwrap();
        assert_eq!(etag_for(dt), "\"1700000000123\"");
    }

    #[test]
    fn check_if_match_passes_on_exact_match() {
        let dt = Utc.timestamp_millis_opt(1_700_000_000_123).unwrap();
        assert_eq!(check_if_match(Some("\"1700000000123\""), dt), Ok(()));
    }

    #[test]
    fn check_if_match_fails_on_stale_value() {
        let dt = Utc.timestamp_millis_opt(1_700_000_000_123).unwrap();
        assert_eq!(check_if_match(Some("\"1699999999999\""), dt), Err(()));
    }

    #[test]
    fn check_if_match_fails_when_missing() {
        let dt = Utc.timestamp_millis_opt(1_700_000_000_123).unwrap();
        assert_eq!(check_if_match(None, dt), Err(()));
    }
}
