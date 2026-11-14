//! Shared pieces of the `/v1` `PATCH` contract (issue #23).
//!
//! Every `PATCH` body is a partial update:
//!
//! - an **omitted** field keeps its current value;
//! - on a nullable field, an explicit **`null` clears** it; on a field the
//!   model requires, `null` is the same as omitting it;
//! - **unknown fields are refused** (`deny_unknown_fields` on each body), so a
//!   misspelled key is a 400 rather than a silent no-op;
//! - a body that changes nothing writes nothing — no `updated_at` bump, so the
//!   ETag stays put, and no audit row.
//!
//! Update bodies are therefore their own types, distinct from the create
//! bodies, where a missing required field is an error.

use serde::Deserialize;

use crate::error::AppError;

/// Deserialize a nullable field that distinguishes *absent* (outer `None`, via
/// `#[serde(default)]`) from *explicitly `null`* (`Some(None)`).
///
/// Use with `#[serde(default, deserialize_with = "present_or_null")]` on an
/// `Option<Option<T>>` field; the result folds onto the current value with
/// `body.field.unwrap_or(current.field)`.
pub(crate) fn present_or_null<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A required text field may be omitted from a `PATCH`, but if it is sent it
/// must not be blank.
pub(crate) fn reject_blank(field: &str, value: Option<&str>) -> Result<(), AppError> {
    match value {
        Some(text) if text.trim().is_empty() => Err(AppError::field(
            field,
            &format!("{field} must not be empty"),
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Body {
        #[serde(default, deserialize_with = "present_or_null")]
        note: Option<Option<String>>,
    }

    fn parse(json: &str) -> Option<Option<String>> {
        serde_json::from_str::<Body>(json).expect("valid body").note
    }

    #[test]
    fn absent_null_and_value_are_three_different_things() {
        assert_eq!(parse("{}"), None);
        assert_eq!(parse(r#"{"note":null}"#), Some(None));
        assert_eq!(parse(r#"{"note":"x"}"#), Some(Some("x".to_string())));
    }

    #[test]
    fn blank_is_refused_only_when_sent() {
        assert!(reject_blank("title", None).is_ok());
        assert!(reject_blank("title", Some("Bolero")).is_ok());
        assert!(reject_blank("title", Some("  ")).is_err());
    }
}
