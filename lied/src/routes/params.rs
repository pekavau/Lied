//! [`ApiPath`] and [`ApiQuery`]: the path- and query-parameter extractors for
//! every `/v1` handler (issue #66), the counterparts of
//! [`ApiJson`](super::json::ApiJson) for bodies.
//!
//! axum's own `Path` and `Query` reject a bad parameter with a plain-text
//! 400, outside the `/v1` rule that every error is RFC 7807 Problem Details.
//! These wrap them and answer with a 400 `validation-failed` naming the
//! parameter in `errors`, keyed by its wire name: the route's `{orgId}`, or
//! the query key (`limit`).
//!
//! A malformed path id is a **400, not a 404**: the request is malformed, and
//! naming the parameter tells the client what to fix, where "not found"
//! would send it looking for a resource that was never addressed.
//!
//! The `/v1` route modules import these under axum's names
//! (`use crate::routes::params::{ApiPath as Path, ApiQuery as Query}`) so a
//! handler signature reads as usual; clippy's `disallowed-types` refuses
//! axum's own `Path` and `Query` outside the console, so a new handler cannot
//! quietly fall back to them.

use axum::extract::path::ErrorKind;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use super::json::{join, named_field, without_value};
use crate::error::AppError;

/// Path parameters, deserialized into `T`; a segment that does not parse is a
/// 400 naming the parameter.
pub(crate) struct ApiPath<T>(pub T);

#[async_trait::async_trait]
impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        #[allow(clippy::disallowed_types)] // the one place axum's `Path` is meant to be used
        let extracted = axum::extract::Path::<T>::from_request_parts(parts, state).await;
        match extracted {
            Ok(axum::extract::Path(value)) => Ok(ApiPath(value)),
            Err(rejection) => Err(path_error(rejection, parts, state).await),
        }
    }
}

async fn path_error<S: Send + Sync>(
    rejection: PathRejection,
    parts: &mut Parts,
    state: &S,
) -> AppError {
    let PathRejection::FailedToDeserializePathParams(err) = rejection else {
        // Only a routing bug (an extractor on a route without parameters).
        return AppError::Internal(anyhow::anyhow!("path parameters unavailable: {rejection}"));
    };
    match err.into_kind() {
        ErrorKind::ParseErrorAtKey {
            key, expected_type, ..
        } => AppError::field(&key, &format!("must be {}", describe(expected_type))),
        // A tuple (`Path<(Uuid, Uuid)>`) reports the position only; the raw
        // parameters, in route order, name it.
        ErrorKind::ParseErrorAtIndex {
            index,
            expected_type,
            ..
        } => {
            let key = RawPathParams::from_request_parts(parts, state)
                .await
                .ok()
                .and_then(|raw| raw.iter().nth(index).map(|(key, _)| key.to_string()))
                .unwrap_or_else(|| "path".to_string());
            AppError::field(&key, &format!("must be {}", describe(expected_type)))
        }
        ErrorKind::ParseError { expected_type, .. } => {
            AppError::field("path", &format!("must be {}", describe(expected_type)))
        }
        ErrorKind::InvalidUtf8InPathParam { key } => AppError::field(&key, "must be valid UTF-8"),
        // A type with its own `Deserialize` — `Uuid`, which every /v1 path
        // parameter is — fails with a bare message: no key, no position.
        // Name the first segment that is not a UUID.
        ErrorKind::Message(message) => {
            let raw = RawPathParams::from_request_parts(parts, state).await.ok();
            let bad_uuid = raw.as_ref().and_then(|raw| {
                raw.iter()
                    .find(|(_, value)| uuid::Uuid::parse_str(value).is_err())
                    .map(|(key, _)| key.to_string())
            });
            match bad_uuid {
                Some(key) => AppError::field(&key, "must be a UUID"),
                None => AppError::field("path", &without_value(&message)),
            }
        }
        // The handler's type and the route disagree: a bug, not a bad request.
        other => AppError::Internal(anyhow::anyhow!(
            "path parameters do not fit the handler: {other}"
        )),
    }
}

/// A readable name for the Rust type a parameter failed to parse as.
fn describe(type_name: &str) -> String {
    let short = type_name.rsplit("::").next().unwrap_or(type_name);
    match short {
        "Uuid" => "a UUID".to_string(),
        "u8" | "u16" | "u32" | "u64" | "usize" => "a whole number, 0 or more".to_string(),
        "i8" | "i16" | "i32" | "i64" | "isize" => "a whole number".to_string(),
        "bool" => "`true` or `false`".to_string(),
        other => format!("a valid {other}"),
    }
}

/// Query parameters, deserialized into `T`; a parameter that does not parse
/// (or a required one that is missing) is a 400 naming it.
pub(crate) struct ApiQuery<T>(pub T);

#[async_trait::async_trait]
impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, AppError> {
        parse_query(parts.uri.query().unwrap_or_default()).map(ApiQuery)
    }
}

/// Deserialize a raw query string as `T`.
pub(crate) fn parse_query<T: DeserializeOwned>(query: &str) -> Result<T, AppError> {
    let deserializer =
        serde_urlencoded::Deserializer::new(form_urlencoded::parse(query.as_bytes()));
    serde_path_to_error::deserialize(deserializer).map_err(|err| {
        let path = err.path().to_string();
        let message = without_value(&err.inner().to_string());
        let field = named_field(&message)
            .map(|name| join(&path, name))
            .unwrap_or_else(|| {
                if path == "." {
                    "query".to_string()
                } else {
                    path
                }
            });
        AppError::field(&field, &message)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[allow(dead_code)]
    struct Q {
        user_id: uuid::Uuid,
        limit: Option<u32>,
        sort: Option<String>,
    }

    fn field_of(query: &str) -> String {
        match parse_query::<Q>(query) {
            Err(AppError::Validation(report)) => {
                let fields: Vec<String> = report.iter().map(|(p, _)| p.to_string()).collect();
                assert_eq!(fields.len(), 1, "{fields:?}");
                fields.into_iter().next().unwrap_or_default()
            }
            other => panic!("{query} → {other:?}"),
        }
    }

    #[test]
    fn query_errors_name_the_parameter() {
        let id = uuid::Uuid::now_v7();
        assert_eq!(field_of(&format!("userId={id}&limit=abc")), "limit");
        assert_eq!(field_of(&format!("userId={id}&limit=-1")), "limit");
        assert_eq!(field_of("limit=5"), "userId");
        assert_eq!(field_of("userId=not-a-uuid"), "userId");
    }

    #[test]
    fn a_good_query_parses_and_ignores_other_keys() {
        let id = uuid::Uuid::now_v7();
        let q: Q = parse_query(&format!("userId={id}&limit=5&filter%5Bstatus%5D=active")).unwrap();
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.user_id, id);
    }

    #[test]
    fn type_names_read_as_words() {
        assert_eq!(describe("uuid::Uuid"), "a UUID");
        assert_eq!(describe("u32"), "a whole number, 0 or more");
        assert_eq!(describe("my::Thing"), "a valid Thing");
    }
}
