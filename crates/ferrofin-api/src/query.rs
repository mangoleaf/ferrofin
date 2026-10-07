//! Case-insensitive query binding shared by every typed API handler.
//!
//! Discover serde's accepted field names (including aliases) without constructing
//! a value. Fold only those names; raw queries retain their spelling. Scalars
//! bind the first occurrence, as ASP.NET's `SimpleTypeModelBinder.FirstValue`
//! does. Only declared collections merge. Values remain percent-encoded until
//! deserialization, so a comma in a scalar is never split.

use std::borrow::Cow;
use std::fmt;
use std::ops::{Deref, DerefMut};

use axum::extract::FromRequestParts;
use axum::http::{StatusCode, Uri, request::Parts};
use axum::response::{IntoResponse, Response};
use serde::de::{self, DeserializeOwned, Visitor};

/// Case-insensitive query binding with first-value scalars and merged collections.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

/// A query binding failure with axum's HTTP 400 status and diagnostic format.
#[derive(Debug, thiserror::Error)]
#[error("Failed to deserialize query string: {0}")]
pub struct QueryRejection(#[source] serde_path_to_error::Error<serde_urlencoded::de::Error>);

impl IntoResponse for QueryRejection {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, self.to_string()).into_response()
    }
}

/// Collection fields carried as delimited strings by a typed query DTO.
///
/// Every other field is scalar. Metadata belongs to the DTO because names such
/// as `id`, `sortBy` and `type` have different cardinality on different routes.
pub trait QueryParameters: DeserializeOwned {
    /// Accepted field names and their collection delimiters (comma or pipe).
    const COLLECTIONS: &'static [(&'static str, char)];
}

impl QueryParameters for Vec<(String, String)> {
    const COLLECTIONS: &'static [(&'static str, char)] = &[];
}

/// Keep each module's cardinality declarations beside its query DTOs. Requiring
/// the trait on the extractor makes a newly added DTO an explicit audit point.
macro_rules! query_parameters {
    ($($ty:ident { $($field:literal => $delimiter:literal),* $(,)? }
        => [$($route:expr),* $(,)?];)*) => {
        $(impl $crate::query::QueryParameters for $ty {
            const COLLECTIONS: &'static [(&'static str, char)] = &[$(($field, $delimiter)),*];
        })*

        #[cfg(test)]
        mod query_contract {
            #[test]
            fn collection_metadata_matches_contract() {
                $($crate::query::tests::assert_contract::<super::$ty>(&[$($route),*]);)*
            }
        }
    };
}

pub(crate) use query_parameters;

impl<T: QueryParameters> Query<T> {
    /// Binds query parameters using the same rules as the HTTP extractor.
    ///
    /// # Errors
    /// Returns an HTTP 400 query rejection when a value cannot bind to `T`.
    pub fn try_from_uri(uri: &Uri) -> Result<Self, QueryRejection> {
        let fields = fields::<T>();
        let query = uri.query().unwrap_or_default();
        let normalized = normalize(query, fields, T::COLLECTIONS);
        // Bind directly: adding '=' to bare parameters can grow an otherwise
        // valid request beyond http::Uri's size limit. Use the same deserializer
        // and error paths as axum without constructing another URI.
        let deserializer = serde_urlencoded::Deserializer::new(form_urlencoded::parse(
            normalized.as_deref().unwrap_or(query).as_bytes(),
        ));
        serde_path_to_error::deserialize(deserializer)
            .map(Self)
            .map_err(QueryRejection)
    }
}

impl<T: QueryParameters + Send, S: Send + Sync> FromRequestParts<S> for Query<T> {
    type Rejection = QueryRejection;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Self::try_from_uri(&parts.uri))
    }
}

impl<T> Deref for Query<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Query<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// Serde calls `deserialize_struct` with aliases as well as declared names.
fn fields<T: DeserializeOwned>() -> &'static [&'static str] {
    match T::deserialize(Capture) {
        Err(Captured(fields)) => fields,
        Ok(_) => &[],
    }
}

/// A deliberate deserialization stop carrying the struct's field names.
#[derive(Debug)]
struct Captured(&'static [&'static str]);

impl fmt::Display for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("query field capture")
    }
}

impl std::error::Error for Captured {}

impl de::Error for Captured {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self(&[])
    }
}

/// Reads schema names without asking serde to visit any values.
struct Capture;

impl<'de> de::Deserializer<'de> for Capture {
    type Error = Captured;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
        Err(Captured(&[]))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error> {
        Err(Captured(fields))
    }

    // Wrappers bind the inner struct's members as well.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map enum
        identifier ignored_any
    }
}

/// Rewrites recognized keys, keeping the first scalar and every collection value.
/// Non-struct queries (e.g. raw pairs) remain untouched. Delimiters describe the
/// handler's existing string representation; commas inside scalar values are data.
fn normalize(query: &str, fields: &[&str], collections: &[(&str, char)]) -> Option<String> {
    if fields.is_empty() || query.is_empty() {
        return None;
    }
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut changed = false;
    for pair in query.split('&') {
        let (raw_key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decoded_key(raw_key);
        // Always pick the same spelling, even if two serde aliases differ only
        // in case. Otherwise `MaxWidth` and `maxWidth` would remain separate.
        let canonical = fields
            .iter()
            .copied()
            .find(|f| f.eq_ignore_ascii_case(&key));
        let Some(key) = canonical else {
            groups.push((pair, Vec::new()));
            continue;
        };
        changed |= key != raw_key;
        if let Some((_, values)) = groups.iter_mut().find(|(k, v)| *k == key && !v.is_empty()) {
            if collections
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(key))
            {
                values.push(value);
            }
            changed = true;
        } else {
            groups.push((key, vec![value]));
        }
    }
    changed.then(|| {
        groups
            .into_iter()
            .map(|(key, values)| {
                if values.is_empty() {
                    key.to_owned()
                } else {
                    let delimiter = collections.iter().find_map(|(name, delimiter)| {
                        name.eq_ignore_ascii_case(key).then_some(*delimiter)
                    });
                    // Keep inserted separators encoded alongside original values.
                    let separator = if delimiter == Some('|') { "%7C" } else { "," };
                    let key: String = form_urlencoded::byte_serialize(key.as_bytes()).collect();
                    format!("{key}={}", values.join(separator))
                }
            })
            .collect::<Vec<_>>()
            .join("&")
    })
}

/// Decode form-encoded names before matching, without decoding their values.
fn decoded_key(key: &str) -> Cow<'_, str> {
    if !key.contains(['%', '+']) {
        return Cow::Borrowed(key);
    }
    form_urlencoded::parse(key.as_bytes())
        .next()
        .map_or(Cow::Borrowed(""), |(key, _)| key)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Query, QueryParameters};

    /// Audit every implemented query field against the newest vendored operation
    /// that declares it. The older contract still covers removed/legacy routes;
    /// newer cardinality takes precedence (e.g. DELETE /Devices now takes ids).
    pub(crate) fn assert_contract<T: super::QueryParameters>(routes: &[(&str, &str)]) {
        use std::sync::OnceLock;
        static SPECS: OnceLock<[serde_json::Value; 2]> = OnceLock::new();
        let specs = SPECS.get_or_init(|| {
            [
                serde_json::from_str(include_str!(
                    "../../../contracts/jellyfin-openapi-12.1.0.json"
                ))
                .unwrap(),
                serde_json::from_str(include_str!(
                    "../../../contracts/jellyfin-openapi-10.11.8.json"
                ))
                .unwrap(),
            ]
        });
        let fields = super::fields::<T>();
        for &(name, delimiter) in T::COLLECTIONS {
            assert!(
                fields.iter().any(|f| f.eq_ignore_ascii_case(name)),
                "{}: unknown collection {name}",
                std::any::type_name::<T>()
            );
            assert!(matches!(delimiter, ',' | '|'));
        }
        for &(method, path) in routes {
            assert!(
                specs
                    .iter()
                    .any(|spec| spec["paths"][path][method].is_object()),
                "unknown operation: {method} {path}"
            );
            for field in fields {
                let param = specs.iter().find_map(|spec| {
                    spec["paths"][path][method]["parameters"]
                        .as_array()?
                        .iter()
                        .find(|p| {
                            p["in"] == "query"
                                && p["name"]
                                    .as_str()
                                    .is_some_and(|name| name.eq_ignore_ascii_case(field))
                        })
                });
                if let Some(param) = param {
                    let collection = T::COLLECTIONS
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(field));
                    assert_eq!(
                        collection,
                        param["schema"]["type"] == "array",
                        "{}: {method} {path}?{field}",
                        std::any::type_name::<T>()
                    );
                }
            }
        }
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Parameters {
        #[serde(alias = "DeviceId")]
        device_id: Option<String>,
        #[serde(rename = "is4K")]
        is_4k: Option<bool>,
    }

    impl super::QueryParameters for Parameters {
        const COLLECTIONS: &'static [(&'static str, char)] = &[];
    }

    #[test]
    fn wrappers_capture_the_inner_fields() {
        #[derive(serde::Deserialize)]
        struct Wrapper(#[allow(dead_code)] Parameters);
        assert_eq!(
            super::fields::<Option<Parameters>>(),
            super::fields::<Parameters>()
        );
        assert_eq!(super::fields::<Wrapper>(), super::fields::<Parameters>());
        assert!(super::fields::<crate::handlers::hls::HlsQueryPub>().contains(&"videoCodec"));
    }

    #[test]
    fn matches_casing_renames_aliases_and_encoded_names() {
        for key in ["deviceId", "DeviceId", "DEVICEID", "deviceid", "%64eviceId"] {
            let uri = format!("/?{key}=a%2Bb%26c&IS4K=true").parse().unwrap();
            let query = Query::<Parameters>::try_from_uri(&uri).unwrap();
            assert_eq!(query.device_id.as_deref(), Some("a+b&c"));
            assert_eq!(query.is_4k, Some(true));
        }
    }

    #[test]
    fn groups_alias_case_variants_before_binding() {
        let uri = "/?DeviceId=a&deviceid=b&deviceId=c".parse().unwrap();
        let query = Query::<Parameters>::try_from_uri(&uri).unwrap();
        assert_eq!(query.device_id.as_deref(), Some("a"));
    }

    #[test]
    fn raw_pairs_keep_names_values_and_repetitions() {
        let uri = "/?a=1&A=2&a=3".parse().unwrap();
        let query = Query::<Vec<(String, String)>>::try_from_uri(&uri).unwrap();
        assert_eq!(
            query.0,
            [
                ("a".into(), "1".into()),
                ("A".into(), "2".into()),
                ("a".into(), "3".into())
            ]
        );
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Mixed {
        name: Option<String>,
        limit: Option<i32>,
        recursive: Option<bool>,
        #[serde(
            default,
            deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
        )]
        user_id: Option<uuid::Uuid>,
        fields: Option<String>,
        genres: Option<String>,
    }

    impl super::QueryParameters for Mixed {
        const COLLECTIONS: &'static [(&'static str, char)] = &[("fields", ','), ("genres", '|')];
    }

    fn mixed(query: &str) -> Result<Query<Mixed>, super::QueryRejection> {
        Query::try_from_uri(&format!("/?{query}").parse().unwrap())
    }

    #[test]
    fn first_scalar_wins_in_wire_order_including_empty_and_encoded_values() {
        for (query, expected) in [
            ("name=a&name=b&name=c", "a"),
            ("NAME=b&name=a", "b"),
            ("name=&NAME=second", ""),
            ("name&name=second", ""),
            ("name=%20&name=second", " "),
            ("name=a,b&name=c", "a,b"),
            ("name=a%2Cb&NAME=c", "a,b"),
            ("na%6De=a%26b%3Dc%2Bd&name=second", "a&b=c+d"),
            ("name=a+b&name=second", "a b"),
            ("name=%252C&name=second", "%2C"),
            ("unknown=x&NAME=%C3%A9&name=second&unknown=y", "é"),
        ] {
            assert_eq!(
                mixed(query).unwrap().name.as_deref(),
                Some(expected),
                "{query}"
            );
        }
    }

    #[test]
    fn typed_scalars_ignore_invalid_later_values_but_reject_an_invalid_first() {
        let id = uuid::Uuid::from_u128(19);
        let query = mixed(&format!(
            "limit=7&LIMIT=nope&recursive=true&RECURSIVE=nope&userId={id}&USERID=nope"
        ))
        .unwrap();
        assert_eq!(query.limit, Some(7));
        assert_eq!(query.recursive, Some(true));
        assert_eq!(query.user_id, Some(id));
        for bad in [
            "limit=nope&LIMIT=7",
            "recursive=nope&recursive=true",
            "userId=nope&userId=00000000-0000-0000-0000-000000000019",
        ] {
            assert!(mixed(bad).is_err(), "{bad}");
        }
        assert_eq!(
            mixed(&format!("userId=&USERID={id}")).unwrap().user_id,
            None
        );
    }

    #[test]
    fn comma_and_pipe_collections_keep_all_occurrences_beside_scalars() {
        let query = mixed("fields=A%2CB&FIELDS=C&fields=D&genres=News%2CSport&GENRES=Drama&name=first&NAME=second").unwrap();
        assert_eq!(query.fields.as_deref(), Some("A,B,C,D"));
        assert_eq!(query.genres.as_deref(), Some("News,Sport|Drama"));
        assert_eq!(query.name.as_deref(), Some("first"));
        let query = mixed("fields=&fields=A&fields=&genres=&genres=Drama").unwrap();
        assert_eq!(query.fields.as_deref(), Some(",A,"));
        assert_eq!(query.genres.as_deref(), Some("|Drama"));
    }

    #[test]
    fn unchanged_queries_and_unknown_parameters_keep_the_fast_path() {
        let fields = super::fields::<Mixed>();
        for query in [
            "",
            "name=a,b",
            "limit=7&fields=A%2CB",
            "unknown=a&unknown=b",
            "flag&&",
        ] {
            assert_eq!(
                super::normalize(query, fields, Mixed::COLLECTIONS),
                None,
                "{query}"
            );
        }
        let query = mixed("").unwrap();
        assert_eq!(query.name, None);
        assert_eq!(query.fields, None);
    }

    #[test]
    fn normalization_handles_queries_at_the_uri_size_limit() {
        let prefix = "/?NAME&fields&genres&unknown=";
        let uri = format!("{prefix}{}", "x".repeat(65_534 - prefix.len()))
            .parse()
            .unwrap();
        let query = Query::<Mixed>::try_from_uri(&uri).unwrap();
        assert_eq!(query.name.as_deref(), Some(""));
        assert_eq!(query.fields.as_deref(), Some(""));
        assert_eq!(query.genres.as_deref(), Some(""));
    }

    #[test]
    fn encoded_names_and_values_are_decoded_exactly_once() {
        #[derive(Debug, serde::Deserialize)]
        struct Encoded {
            #[serde(rename = "a+b&c=d %")]
            value: String,
        }
        impl QueryParameters for Encoded {
            const COLLECTIONS: &'static [(&'static str, char)] = &[];
        }
        let uri = "/?a%2Bb%26c%3Dd+%25=first%2526&a%2Bb%26c%3Dd%20%25=second"
            .parse()
            .unwrap();
        assert_eq!(
            Query::<Encoded>::try_from_uri(&uri).unwrap().value,
            "first%26"
        );
        for (query, expected) in [
            ("na%256De=ignored&name=first&NAME=second", "first"),
            ("name%=ignored&NAME=first&name=second", "first"),
            ("name=bad%ZZ%4&name=second", "bad%ZZ%4"),
            ("name=%FF&name=second", "\u{fffd}"),
        ] {
            assert_eq!(mixed(query).unwrap().name.as_deref(), Some(expected));
        }
    }

    #[tokio::test]
    async fn invalid_queries_preserve_axums_rejection_response() {
        use axum::response::IntoResponse;

        for (input, first_only) in [
            ("LIMIT=bad&limit=7", "limit=bad"),
            ("RECURSIVE=&recursive=true", "recursive="),
            ("USERID=invalid&userId=", "userId=invalid"),
        ] {
            let actual = mixed(input).unwrap_err().into_response();
            let expected = axum::extract::Query::<Mixed>::try_from_uri(
                &format!("/?{first_only}").parse().unwrap(),
            )
            .unwrap_err()
            .into_response();
            assert_eq!(actual.status(), expected.status());
            assert_eq!(actual.headers(), expected.headers());
            assert_eq!(
                axum::body::to_bytes(actual.into_body(), usize::MAX)
                    .await
                    .unwrap(),
                axum::body::to_bytes(expected.into_body(), usize::MAX)
                    .await
                    .unwrap()
            );
        }
    }

    #[tokio::test]
    async fn extraction_preserves_the_original_uri_for_raw_consumers() {
        use axum::extract::FromRequestParts;
        let uri = "/?NAME=first&name=second&fields=A&FIELDS=B";
        let (mut parts, ()) = axum::http::Request::builder()
            .uri(uri)
            .body(())
            .unwrap()
            .into_parts();
        let query = Query::<Mixed>::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(query.name.as_deref(), Some("first"));
        assert_eq!(query.fields.as_deref(), Some("A,B"));
        assert_eq!(parts.uri.to_string(), uri);
    }
}
