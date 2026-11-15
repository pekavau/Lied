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
//! [`ApiListQuery`] is `ApiQuery` for list endpoints: the same control
//! parameters plus the `filter[…]` pairs, decoded in **one** pass (#69), so a
//! filter key or value can never decode differently from `limit` or `q`.
//!
//! The `/v1` route modules import these under axum's names
//! (`use crate::routes::params::{ApiPath as Path, ApiQuery as Query}`) so a
//! handler signature reads as usual; clippy's `disallowed-types` refuses
//! axum's own `Path` and `Query` outside the console, so a new handler cannot
//! quietly fall back to them.

use axum::extract::path::ErrorKind;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRequestParts, RawPathParams};
use std::borrow::Cow;

use axum::http::request::Parts;
use serde::de::value::{Error as DeError, MapDeserializer};
use serde::de::{DeserializeOwned, IntoDeserializer};

use super::json::{field_error, without_value};
use crate::error::AppError;
use crate::listing::RawFilters;

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
        // Name the first segment that is not a UUID. That every /v1 path
        // parameter is a UUID is pinned by `every_path_parameter_is_a_uuid`
        // in tests/openapi.rs; a non-UUID one needs naming here first.
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

/// A list endpoint's query: the control parameters (`limit`, `offset`,
/// `sort`, `q`, …) deserialized into `T`, and the raw `filter[…]` pairs for
/// [`listing::resolve_filters`](crate::listing::resolve_filters).
///
/// Both come from one decoding pass over the query string; see
/// [`split_query`] for the rules.
pub(crate) struct ApiListQuery<T> {
    pub params: T,
    pub filters: RawFilters,
}

#[async_trait::async_trait]
impl<T, S> FromRequestParts<S> for ApiListQuery<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, AppError> {
        parse_list_query(parts.uri.query().unwrap_or_default())
    }
}

/// Deserialize a raw query string as `T`. `filter[…]` keys are not passed to
/// `T`: a plain `ApiQuery` ignores them, as it ignores any key it does not
/// name.
pub(crate) fn parse_query<T: DeserializeOwned>(query: &str) -> Result<T, AppError> {
    let (controls, _filters) = split_query(query)?;
    deserialize_pairs(controls)
}

/// [`ApiListQuery`]'s parser, separate from the extractor for unit tests.
pub(crate) fn parse_list_query<T: DeserializeOwned>(
    query: &str,
) -> Result<ApiListQuery<T>, AppError> {
    let (controls, filters) = split_query(query)?;
    Ok(ApiListQuery {
        params: deserialize_pairs(controls)?,
        filters,
    })
}

type Pair<'a> = (Cow<'a, str>, Cow<'a, str>);

/// The one decoding pass every `/v1` query goes through:
/// `application/x-www-form-urlencoded` rules (`+` is a space, `%XX`
/// escapes, invalid UTF-8 replaced), applied to keys and values alike.
///
/// - Keys are decoded **before** the `filter[` prefix is matched: a browser
///   submitting a GET form sends `filter%5Bstatus%5D`, and matching the raw
///   key would silently drop the filter.
/// - A key without `=` is a key with an empty value, for filters as for
///   control parameters: `filter[status]` means `filter[status]=` (which the
///   endpoints read as "no filter"). It is never dropped, so an unknown one
///   still reaches the allowlist and is refused.
/// - A filter named twice is a 400 naming it, as a duplicated control
///   parameter is: keeping either value would silently drop the other.
fn split_query(query: &str) -> Result<(Vec<Pair<'_>>, RawFilters), AppError> {
    let mut controls = Vec::new();
    let mut filters = RawFilters::new();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        let field = key
            .strip_prefix("filter[")
            .and_then(|rest| rest.strip_suffix(']'));
        match field {
            Some(field) => {
                if filters
                    .insert(field.to_string(), value.into_owned())
                    .is_some()
                {
                    return Err(field_error(
                        &format!("filter[{field}]"),
                        "given more than once",
                        "query",
                    ));
                }
            }
            None => controls.push((key, value)),
        }
    }
    Ok((controls, filters))
}

/// Deserialize already-decoded pairs as `T`, naming the key that fails.
fn deserialize_pairs<T: DeserializeOwned>(pairs: Vec<Pair<'_>>) -> Result<T, AppError> {
    let deserializer =
        MapDeserializer::<_, DeError>::new(pairs.into_iter().map(|(k, v)| (Part(k), Part(v))));
    serde_path_to_error::deserialize(deserializer)
        .map_err(|err| field_error(&err.path().to_string(), &err.inner().to_string(), "query"))
}

/// One decoded key or value. Strings deserialize as they are; numbers and
/// booleans parse from their text, as a form value does (the rules of
/// `serde_urlencoded`, which `ApiQuery` used before #69).
struct Part<'a>(Cow<'a, str>);

impl<'de> IntoDeserializer<'de, DeError> for Part<'de> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

macro_rules! parse_from_text {
    ($($ty:ty => $method:ident,)*) => {
        $(
            fn $method<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
                match self.0.parse::<$ty>() {
                    Ok(value) => serde::Deserializer::$method(
                        IntoDeserializer::<DeError>::into_deserializer(value),
                        visitor,
                    ),
                    Err(err) => Err(serde::de::Error::custom(err)),
                }
            }
        )*
    };
}

impl<'de> serde::Deserializer<'de> for Part<'de> {
    type Error = DeError;

    fn deserialize_any<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.0 {
            Cow::Borrowed(text) => visitor.visit_borrowed_str(text),
            Cow::Owned(text) => visitor.visit_string(text),
        }
    }

    fn deserialize_option<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    /// Unit variants only, named by the value (`?status=active`).
    fn deserialize_enum<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        serde::Deserializer::deserialize_enum(
            IntoDeserializer::<DeError>::into_deserializer(self.0.into_owned()),
            name,
            variants,
            visitor,
        )
    }

    parse_from_text! {
        bool => deserialize_bool,
        u8 => deserialize_u8,
        u16 => deserialize_u16,
        u32 => deserialize_u32,
        u64 => deserialize_u64,
        i8 => deserialize_i8,
        i16 => deserialize_i16,
        i32 => deserialize_i32,
        i64 => deserialize_i64,
        f32 => deserialize_f32,
        f64 => deserialize_f64,
    }

    serde::forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct
        map struct identifier ignored_any
    }
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
    fn a_duplicated_key_is_named_and_a_bad_uuid_is_not_echoed() {
        let id = uuid::Uuid::now_v7();
        assert_eq!(field_of(&format!("userId={id}&limit=5&limit=6")), "limit");
        let Err(AppError::Validation(report)) = parse_query::<Q>("userId=n0t-a-uuid") else {
            panic!("expected a validation error");
        };
        let rendered: Vec<String> = report.iter().map(|(_, e)| e.to_string()).collect();
        assert!(!rendered.iter().any(|m| m.contains("`n`")), "{rendered:?}");
    }

    #[test]
    fn a_good_query_parses_and_ignores_other_keys() {
        let id = uuid::Uuid::now_v7();
        let q: Q = parse_query(&format!("userId={id}&limit=5&filter%5Bstatus%5D=active")).unwrap();
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.user_id, id);
    }

    #[derive(Debug, Deserialize)]
    struct ListQ {
        q: Option<String>,
        limit: Option<u32>,
    }

    fn list(query: &str) -> ApiListQuery<ListQ> {
        match parse_list_query::<ListQ>(query) {
            Ok(parsed) => parsed,
            Err(err) => panic!("{query} → {err:?}"),
        }
    }

    fn filter<'a>(parsed: &'a ApiListQuery<ListQ>, field: &str) -> Option<&'a str> {
        parsed.filters.get(field).map(String::as_str)
    }

    /// #69: control parameters and filters go through one decoder, so the
    /// same text decodes the same way in both: `+` is a space, `%2B` a plus,
    /// a key without `=` an empty value, a bad escape kept, bad UTF-8
    /// replaced. Before, `filter[…]` had its own decoder, which dropped a
    /// key without `=` (and so an unknown one never reached the allowlist).
    #[test]
    fn control_parameters_and_filters_decode_alike() {
        for (raw, decoded) in [
            ("a+b%2Bc", "a b+c"),
            ("%zz", "%zz"),
            ("%FF", "\u{FFFD}"),
            ("caf%C3%A9", "café"),
        ] {
            let parsed = list(&format!("q={raw}&filter[name]={raw}"));
            assert_eq!(parsed.params.q.as_deref(), Some(decoded), "q={raw}");
            assert_eq!(filter(&parsed, "name"), Some(decoded), "filter={raw}");
        }

        let bare = list("q&filter[name]");
        assert_eq!(bare.params.q.as_deref(), Some(""));
        assert_eq!(
            filter(&bare, "name"),
            Some(""),
            "a filter key without `=` must not vanish"
        );
    }

    #[test]
    fn filter_keys_are_decoded_before_the_prefix_is_matched() {
        // A browser GET form encodes the brackets; a hand-written URL does
        // not. Both must reach the same filter.
        let literal = list("filter[status]=active&limit=50");
        let encoded = list("filter%5Bstatus%5D=active&limit=50");
        assert_eq!(filter(&literal, "status"), Some("active"));
        assert_eq!(literal.filters, encoded.filters);
        assert_eq!(encoded.params.limit, Some(50));

        let mixed =
            list("filter%5BdurationMaxSeconds%5D=300&filter[tag]=a%2Cb&filter[name]=foo+bar");
        assert_eq!(filter(&mixed, "durationMaxSeconds"), Some("300"));
        assert_eq!(filter(&mixed, "tag"), Some("a,b"));
        assert_eq!(filter(&mixed, "name"), Some("foo bar"));
        assert_eq!(mixed.filters.len(), 3);
    }

    #[test]
    fn filters_never_reach_the_control_parameters() {
        // `filter[limit]` is a filter named `limit`, not the page size.
        let parsed = list("filter[limit]=abc&limit=5");
        assert_eq!(parsed.params.limit, Some(5));
        assert_eq!(filter(&parsed, "limit"), Some("abc"));
    }

    #[test]
    fn a_filter_given_twice_is_named() {
        match parse_list_query::<ListQ>("filter[status]=active&filter%5Bstatus%5D=archived") {
            Err(AppError::Validation(report)) => {
                let fields: Vec<String> = report.iter().map(|(p, _)| p.to_string()).collect();
                assert_eq!(fields, vec!["filter[status]".to_string()]);
            }
            other => panic!("expected a validation error, got {:?}", other.err()),
        }
    }

    #[test]
    fn type_names_read_as_words() {
        assert_eq!(describe("uuid::Uuid"), "a UUID");
        assert_eq!(describe("u32"), "a whole number, 0 or more");
        assert_eq!(describe("my::Thing"), "a valid Thing");
    }
}
