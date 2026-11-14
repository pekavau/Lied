//! [`ApiJson`]: the request-body extractor for every `/v1` JSON body
//! (issue #62).
//!
//! axum's own `Json` extractor rejects a bad body with a plain-text response
//! (400 for broken JSON, 422 for a body of the wrong shape, 415 for a missing
//! `Content-Type`), which breaks the `/v1` rule that every error is RFC 7807
//! Problem Details. `ApiJson` turns each of those into an [`AppError`]:
//!
//! | Failure | Status | Problem type |
//! |---|---|---|
//! | `Content-Type` is not JSON | 415 | `unsupported-media-type` |
//! | body larger than `LIED_MAX_REQUEST_BYTES` | 413 | `payload-too-large` |
//! | not valid JSON (syntax, truncated, empty) | 400 | `bad-request` |
//! | valid JSON of the wrong shape — missing, unknown or wrong-typed field | 400 | `validation-failed`, with the field in `errors` |
//!
//! The shape errors are field errors like a garde validation failure, keyed
//! by the field's wire (camelCase) path, so a client handles "you sent a bad
//! `difficulty`" the same way whichever layer noticed it.

use axum::extract::{FromRequest, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderMap;
use serde::de::DeserializeOwned;

use crate::error::AppError;
use crate::state::AppState;

/// A JSON request body, deserialized into `T`; a body that cannot be is an
/// [`AppError`] (see the module docs for which).
pub(crate) struct ApiJson<T>(pub T);

#[async_trait::async_trait]
impl<T: DeserializeOwned> FromRequest<AppState> for ApiJson<T> {
    type Rejection = AppError;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, AppError> {
        if !is_json(request.headers()) {
            return Err(AppError::UnsupportedMediaType(
                "the request body must be JSON (`Content-Type: application/json`)".to_string(),
            ));
        }
        let limit = usize::try_from(state.config.max_request_bytes).unwrap_or(usize::MAX);
        // `Limited` is applied here, from this crate's own http-body-util, so
        // the length error it raises is the type `read_error` looks for.
        let body = http_body_util::Limited::new(request.into_body(), limit);
        let bytes = http_body_util::BodyExt::collect(body)
            .await
            .map_err(|err| read_error(&*err, limit))?
            .to_bytes();
        parse(&bytes).map(ApiJson)
    }
}

/// `application/json`, or any `+json` media type, ignoring parameters such as
/// `charset`.
fn is_json(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let essence = value.split(';').next().unwrap_or_default().trim();
    essence.eq_ignore_ascii_case("application/json")
        || essence.rsplit_once('+').is_some_and(|(kind, suffix)| {
            suffix.eq_ignore_ascii_case("json")
                && kind.to_ascii_lowercase().starts_with("application/")
        })
}

/// A body that could not be read: too large (413), or broken off mid-stream.
fn read_error(err: &(dyn std::error::Error + Send + Sync + 'static), limit: usize) -> AppError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(cause) = source {
        if cause.is::<http_body_util::LengthLimitError>() {
            return AppError::PayloadTooLarge(format!(
                "the request body exceeds the {limit}-byte limit (LIED_MAX_REQUEST_BYTES)"
            ));
        }
        source = cause.source();
    }
    AppError::BadRequest("the request body could not be read".to_string())
}

/// Deserialize `bytes` as `T`, mapping a failure to the Problem Details the
/// module docs describe.
pub(crate) fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, AppError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(shape_or_syntax)?;
    // Trailing content after the value (`{}{}`) is broken JSON too.
    deserializer.end().map_err(|err| syntax_error(&err))?;
    Ok(value)
}

fn shape_or_syntax(err: serde_path_to_error::Error<serde_json::Error>) -> AppError {
    let path = err.path().to_string();
    let inner = err.into_inner();
    if !inner.is_data() {
        return syntax_error(&inner);
    }
    field_error(&path, &without_position(&inner.to_string()), "body")
}

/// The field error for a serde_path_to_error failure at `path` with serde's
/// `message`, shared by bodies ([`ApiJson`]) and queries
/// ([`ApiQuery`](super::params::ApiQuery)). The value the client sent is cut
/// from the message; a field named by the message (missing, unknown,
/// duplicate) is the key, else the failing path, else `fallback` (the root).
pub(super) fn field_error(path: &str, message: &str, fallback: &str) -> AppError {
    let message = without_value(message);
    // A missing or unknown field is reported against its parent; name the
    // field itself, so `errors` points at what the client has to fix.
    let field = named_field(&message)
        .map(|name| join(path, name))
        .unwrap_or_else(|| {
            if path == "." {
                fallback.to_string()
            } else {
                path.to_string()
            }
        });
    AppError::field(&field, &message)
}

fn syntax_error(err: &serde_json::Error) -> AppError {
    let detail = if err.is_eof() {
        "the request body is empty or cut off".to_string()
    } else {
        format!(
            "the request body is not valid JSON: {}",
            without_position(&err.to_string())
        )
    };
    AppError::BadRequest(detail)
}

/// The field a "missing field `x`" / "unknown field `x`, expected …" /
/// "duplicate field `x`" message is about.
pub(super) fn named_field(message: &str) -> Option<&str> {
    let rest = ["missing field `", "unknown field `", "duplicate field `"]
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix))?;
    rest.split_once('`').map(|(name, _)| name)
}

/// The path to `field`. serde_path_to_error already ends the path with an
/// unknown field's name, but leaves a missing field's path at its parent.
pub(super) fn join(path: &str, field: &str) -> String {
    if path == "." {
        field.to_string()
    } else if path == field || path.ends_with(&format!(".{field}")) {
        path.to_string()
    } else {
        format!("{path}.{field}")
    }
}

/// serde's type and value errors quote what the client sent (``invalid type:
/// integer `123456`, expected a string``). Drop the quoted value: the client
/// knows what it sent, and the body may be a credential (`POST /v1/tokens`)
/// that would otherwise come back in the response and in any log of it.
/// Field names in "missing/unknown field" messages are keys, not values, and
/// stay.
pub(super) fn without_value(message: &str) -> String {
    // A type's own parser may quote a piece of the value in passing — uuid's
    // "UUID parsing failed: invalid character: found `n` at 0". Cut it there.
    let message = match message.find("found `") {
        Some(at) => message[..at].trim_end_matches([' ', ',', ':']),
        None => message,
    };
    let quotes_a_value = [
        "invalid type: ",
        "invalid value: ",
        "unknown variant ",
        "invalid length ",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix));
    if !quotes_a_value {
        return message.to_string();
    }
    match message.split_once('`') {
        Some((head, rest)) => match rest.split_once('`') {
            Some((_value, tail)) => format!("{}{}", head.trim_end(), tail),
            None => message.to_string(),
        },
        None => message.to_string(),
    }
}

/// serde_json appends " at line L column C"; the field path already says where.
fn without_position(message: &str) -> String {
    match message.rsplit_once(" at line ") {
        Some((head, tail))
            if tail
                .split(" column ")
                .all(|n| n.chars().all(|c| c.is_ascii_digit())) =>
        {
            head.to_string()
        }
        _ => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    #[allow(dead_code)]
    struct Body {
        title: String,
        difficulty: Option<i16>,
        ratings: Option<Ratings>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Ratings {
        abrsm: String,
    }

    /// The `(status, field)` a body is refused with; `field` is `None` for a
    /// syntax error.
    fn refusal(json: &str) -> (u16, Option<String>) {
        match parse::<Body>(json.as_bytes()) {
            Ok(body) => panic!("{json} was accepted: {body:?}"),
            Err(AppError::Validation(report)) => {
                let fields: Vec<String> = report.iter().map(|(p, _)| p.to_string()).collect();
                assert_eq!(fields.len(), 1, "{fields:?}");
                (400, fields.into_iter().next())
            }
            Err(AppError::BadRequest(_)) => (400, None),
            Err(other) => panic!("{json} → unexpected {other:?}"),
        }
    }

    #[test]
    fn shape_errors_name_the_field() {
        assert_eq!(refusal("{}"), (400, Some("title".into())));
        assert_eq!(
            refusal(r#"{"title":"x","publsher":1}"#),
            (400, Some("publsher".into()))
        );
        assert_eq!(
            refusal(r#"{"title":"x","difficulty":"hard"}"#),
            (400, Some("difficulty".into()))
        );
        assert_eq!(
            refusal(r#"{"title":"x","ratings":{"henle":"5"}}"#),
            (400, Some("ratings.henle".into()))
        );
        assert_eq!(refusal(r#""just a string""#), (400, Some("body".into())));
    }

    #[test]
    fn broken_json_is_a_plain_bad_request() {
        for json in ["", "{", r#"{"title":"x",}"#, r#"{"title":"x"}{}"#] {
            assert_eq!(refusal(json), (400, None), "{json:?}");
        }
    }

    #[test]
    fn messages_do_not_echo_the_sent_value() {
        assert_eq!(
            without_value("invalid type: integer `123456`, expected a string"),
            "invalid type: integer, expected a string"
        );
        assert_eq!(
            without_value("unknown variant `cut`, expected one of `tempo`, `dynamics`"),
            "unknown variant, expected one of `tempo`, `dynamics`"
        );
        assert_eq!(
            without_value("unknown field `publsher`, expected `title`"),
            "unknown field `publsher`, expected `title`"
        );
        assert_eq!(
            without_value(
                "invalid character: expected an optional prefix of `urn:uuid:` followed by \
                 [0-9a-fA-F-], found `n` at 1"
            ),
            "invalid character: expected an optional prefix of `urn:uuid:` followed by \
             [0-9a-fA-F-]"
        );
        let Err(AppError::Validation(report)) = parse::<Body>(br#"{"title":12345678}"#) else {
            panic!("expected a validation error");
        };
        let rendered: Vec<String> = report.iter().map(|(_, e)| e.to_string()).collect();
        assert!(
            !rendered.iter().any(|m| m.contains("12345678")),
            "{rendered:?}"
        );
    }

    #[test]
    fn messages_drop_the_line_and_column() {
        assert_eq!(
            without_position("missing field `title` at line 1 column 2"),
            "missing field `title`"
        );
        assert_eq!(
            without_position("expected i16 at the top"),
            "expected i16 at the top"
        );
    }

    #[test]
    fn json_media_types() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, value.parse().unwrap());
            is_json(&headers)
        };
        assert!(with("application/json"));
        assert!(with("Application/JSON; charset=utf-8"));
        assert!(with("application/merge-patch+json"));
        assert!(!with("text/plain"));
        assert!(!with("application/x-www-form-urlencoded"));
        assert!(!is_json(&HeaderMap::new()));
    }
}
