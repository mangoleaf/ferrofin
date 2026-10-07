//! Case-insensitive query binding shared by every typed API handler.
//!
//! Discover serde's accepted field names (including aliases) without constructing
//! a value. Fold only those names; raw queries and dictionary keys retain their
//! spelling. Values remain percent-encoded until axum performs deserialization.

use std::borrow::Cow;
use std::fmt;
use std::ops::{Deref, DerefMut};

use axum::extract::FromRequestParts;
use axum::extract::rejection::QueryRejection;
use axum::http::{Uri, request::Parts};
use serde::de::{self, DeserializeOwned, Visitor};

/// A query extractor accepting any ASCII casing of a struct's field names.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<T: DeserializeOwned> Query<T> {
    /// Binds query parameters using the same rules as the HTTP extractor.
    ///
    /// # Errors
    /// Returns axum's query rejection when a value cannot bind to `T`.
    pub fn try_from_uri(uri: &Uri) -> Result<Self, QueryRejection> {
        let fields = fields::<T>();
        let normalized = normalize(uri.query().unwrap_or_default(), fields);
        let normalized_uri;
        let uri = if let Some(query) = normalized {
            // Only field names and separators change. Values remain URI-safe.
            normalized_uri = format!("/?{query}").parse::<Uri>().expect("encoded query");
            &normalized_uri
        } else {
            uri
        };
        axum::extract::Query::<T>::try_from_uri(uri).map(|query| Self(query.0))
    }
}

impl<T: DeserializeOwned, S: Send + Sync> FromRequestParts<S> for Query<T> {
    type Rejection = QueryRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Self::try_from_uri(&parts.uri)
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

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}

/// Rewrites recognized keys and merges repetitions in first-occurrence order.
/// Non-struct queries (e.g. raw pairs) remain untouched.
fn normalize(query: &str, fields: &[&str]) -> Option<String> {
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
            values.push(value);
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
                    format!("{key}={}", values.join(","))
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
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(key).expect("string pairs");
    Cow::Owned(
        pairs
            .into_iter()
            .next()
            .map_or_else(String::new, |(key, _)| key),
    )
}

#[cfg(test)]
mod tests {
    use super::Query;

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Parameters {
        #[serde(alias = "DeviceId")]
        device_id: Option<String>,
        #[serde(rename = "is4K")]
        is_4k: Option<bool>,
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
        assert_eq!(query.device_id.as_deref(), Some("a,b,c"));
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
}
