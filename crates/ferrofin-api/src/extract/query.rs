//! The query-string binder: `axum`'s `Query` with ASP.NET's key matching.
//!
//! ASP.NET's query value provider looks keys up **ignoring case**, so Jellyfin
//! binds `?isLocked=true`, `?IsLocked=true` and `?islocked=true` alike. Measured
//! on a live Jellyfin 12.2.0, `GET /Sessions?deviceid=x` and `?DEVICEID=x`
//! filter exactly as `?deviceId=x` does. Ferrofin's typed query structs name
//! their members once (`rename_all = "camelCase"`), so [`Query`] canonicalises
//! each key to the member it equals ignoring ASCII case before `axum`'s own
//! binder runs. The member names come from the struct itself: serde hands
//! them (aliases included) to `deserialize_struct`, which [`members`] captures.
//!
//! Folding can make two keys equal (`ParentId=a&parentid=b`); ASP.NET reads
//! those as one repeated key, so they are merged the way the router merges an
//! exact repeat (`crate::router`'s `merged_query`).

use axum::extract::FromRequestParts;
use axum::extract::rejection::QueryRejection;
use axum::http::Uri;
use axum::http::request::Parts;
use serde::de::{self, DeserializeOwned, Visitor};
use serde::forward_to_deserialize_any;

use crate::router::merged_query;

/// The typed query-string extractor — `axum::extract::Query` with member names
/// matched ignoring ASCII case, as ASP.NET's query binding does.
///
/// Rejections are `axum`'s own [`QueryRejection`] (400, unchanged).
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<T: DeserializeOwned> Query<T> {
    /// Binds `uri`'s query string into `T`, folding its keys to `T`'s member
    /// names first.
    ///
    /// Expects a URI the router has already passed through its
    /// repeated-key merge (`crate::router`'s `merge_repeated_query_params`), as
    /// every request has: only repeats that the fold itself creates are merged
    /// here, so a direct call with an exact repeat (`a=1&a=2`) is rejected as a
    /// duplicate field.
    ///
    /// # Errors
    ///
    /// `axum`'s [`QueryRejection`] when the (folded) query does not bind.
    pub fn try_from_uri(uri: &Uri) -> Result<Self, QueryRejection> {
        // Rebuilt only when the fold changed something; the rename keeps every
        // key a plain identifier, so the rebuilt URI always parses.
        let folded = uri
            .query()
            .and_then(|query| fold_keys(query, members::<T>()))
            .and_then(|query| format!("/?{query}").parse::<Uri>().ok());
        // The one place axum's case-sensitive binder may be used: on keys
        // already folded to the member names.
        #[allow(clippy::disallowed_types)]
        let axum::extract::Query(value) =
            axum::extract::Query::try_from_uri(folded.as_ref().unwrap_or(uri))?;
        Ok(Self(value))
    }
}

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for Query<T> {
    type Rejection = QueryRejection;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Self::try_from_uri(&parts.uri))
    }
}

/// Rewrites each key of a raw query string to the member it equals ignoring
/// ASCII case, then merges any keys that made equal. `None` when nothing
/// changed (the common case).
///
/// A struct's members are unique ignoring case (debug-checked in [`members`]),
/// so a key folds to at most one name. Values, unknown keys and
/// percent-encoding are left as written.
///
/// ponytail: keys are compared as written, so a percent-encoded key (`%55serId`)
/// is not folded; no client encodes key names. Decode here if one ever does.
fn fold_keys(query: &str, members: &[&'static str]) -> Option<String> {
    fn key_of(pair: &str) -> &str {
        pair.split_once('=').map_or(pair, |(key, _)| key)
    }

    // The canonical spelling of `key`, when it names a member.
    let canonical = |key: &str| {
        members
            .iter()
            .find(|m| m.eq_ignore_ascii_case(key))
            .copied()
    };
    // Fast path, no allocation: every key already canonical. Then no two keys
    // were made equal either, and exact repeats were merged by the router.
    if query
        .split('&')
        .all(|pair| canonical(key_of(pair)).is_none_or(|m| m == key_of(pair)))
    {
        return None;
    }
    let folded = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => format!("{}={value}", canonical(key).unwrap_or(key)),
            None => canonical(pair).unwrap_or(pair).to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&");
    Some(merged_query(&folded).unwrap_or(folded))
}

/// `T`'s member names (aliases included), as its derived `Deserialize` hands
/// them to `deserialize_struct`; empty when `T` is not a plain struct (a map,
/// or a struct with a `flatten` member), whose keys are then left as written.
///
/// Two names equal ignoring case would make every spelling bind the first one
/// (ASP.NET could not tell them apart either), so debug builds refuse them; the
/// contract-wide `query_casing` test sends a query string to every contract
/// operation it can reach, which runs this check on each query struct.
fn members<T: DeserializeOwned>() -> &'static [&'static str] {
    let members = match T::deserialize(Capture) {
        Err(Captured(members)) => members,
        Ok(_) => &[],
    };
    debug_assert!(
        !has_case_collision(members),
        "query struct {} has members equal ignoring case: {members:?}",
        std::any::type_name::<T>()
    );
    members
}

/// Whether two of `members` are equal ignoring ASCII case.
fn has_case_collision(members: &[&str]) -> bool {
    members
        .iter()
        .enumerate()
        .any(|(i, a)| members[i + 1..].iter().any(|b| a.eq_ignore_ascii_case(b)))
}

/// The "error" [`Capture`] stops with, carrying the captured member names.
#[derive(Debug)]
struct Captured(&'static [&'static str]);

impl std::fmt::Display for Captured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("member names captured")
    }
}

impl std::error::Error for Captured {}

impl de::Error for Captured {
    fn custom<M: std::fmt::Display>(_msg: M) -> Self {
        Self(&[])
    }
}

/// A deserializer that binds nothing: it records the member names a struct
/// asks for and stops.
struct Capture;

impl<'de> de::Deserializer<'de> for Capture {
    type Error = Captured;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Captured> {
        Err(Captured(&[]))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Captured> {
        Err(Captured(fields))
    }

    // `Option<S>` and a newtype around `S` bind `S`'s members.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Captured> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Captured> {
        visitor.visit_newtype_struct(self)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map enum
        identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ItemsQuery {
        #[serde(default)]
        is_locked: Option<bool>,
        #[serde(default, rename = "is4K")]
        is_4k: Option<bool>,
        #[serde(default, alias = "hd")]
        is_hd: Option<bool>,
        #[serde(default)]
        parent_id: Option<String>,
        #[serde(default)]
        include_item_types: Option<String>,
    }

    fn bind<T: DeserializeOwned>(uri: &str) -> Result<T, QueryRejection> {
        Query::try_from_uri(&uri.parse().expect("uri")).map(|Query(value)| value)
    }

    #[test]
    fn members_include_aliases() {
        assert_eq!(
            members::<ItemsQuery>(),
            [
                "isLocked",
                "is4K",
                "hd",
                "isHd",
                "parentId",
                "includeItemTypes"
            ]
        );
        assert!(members::<HashMap<String, String>>().is_empty());
    }

    #[test]
    fn every_casing_binds() {
        for uri in [
            "/Items?isLocked=true&is4K=true&isHd=true&parentId=p",
            "/Items?IsLocked=true&Is4K=true&IsHD=true&ParentId=p",
            "/Items?islocked=true&is4k=true&ishd=true&parentid=p",
            "/Items?ISLOCKED=true&IS4K=true&ISHD=true&PARENTID=p",
        ] {
            let query: ItemsQuery = bind(uri).expect(uri);
            assert_eq!(query.is_locked, Some(true), "{uri}");
            assert_eq!(query.is_4k, Some(true), "{uri}");
            assert_eq!(query.is_hd, Some(true), "{uri}");
            assert_eq!(query.parent_id.as_deref(), Some("p"), "{uri}");
        }
    }

    #[test]
    fn keys_the_fold_makes_equal_merge_like_a_repeat() {
        let query: ItemsQuery =
            bind("/Items?IncludeItemTypes=Movie&includeitemtypes=Series").expect("binds");
        assert_eq!(query.include_item_types.as_deref(), Some("Movie,Series"));
    }

    #[test]
    fn values_unknown_keys_and_maps_are_untouched() {
        let query: ItemsQuery = bind("/Items?ParentId=Ab%20C&Unknown=1").expect("binds");
        assert_eq!(query.parent_id.as_deref(), Some("Ab C"));
        let map: HashMap<String, String> = bind("/x?MyKey=a&mykey=b").expect("map");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn spellings_of_one_member_land_on_one_key() {
        // Every casing of `isHd` folds to that one name and merges as a
        // repeated key, rather than reaching serde as a duplicate field. How a
        // repeated scalar binds is PLAN_QUERY_SCALAR_DUPLICATES.md.
        assert_eq!(
            fold_keys("IsHd=1&ISHD=2", members::<ItemsQuery>()).as_deref(),
            Some("isHd=1,2")
        );
        let query: ItemsQuery = bind("/Items?HD=true").expect("alias binds");
        assert_eq!(query.is_hd, Some(true));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "equal ignoring case")]
    fn a_case_only_alias_is_refused_in_debug_builds() {
        // Redundant now that keys fold, and indistinguishable from two members
        // that collide; the guard keeps query structs free of both.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        #[allow(dead_code)]
        struct CaseAlias {
            #[serde(default, alias = "isHD")]
            is_hd: Option<bool>,
        }
        let _ = members::<CaseAlias>();
    }

    #[test]
    fn the_fold_keeps_the_raw_query_text() {
        let members = members::<ItemsQuery>();
        assert_eq!(
            fold_keys("parentId=a&x=1", members),
            None,
            "nothing changed"
        );
        assert_eq!(
            fold_keys("ParentId=a+b%2Cc&IsLocked&&", members).as_deref(),
            // The merge folds the two empty pairs into one; both are ignored.
            Some("parentId=a+b%2Cc&isLocked&")
        );
        let query: ItemsQuery = bind("/Items?ParentId=a+b%2Cc").expect("binds");
        assert_eq!(query.parent_id.as_deref(), Some("a b,c"));
    }

    #[test]
    fn wrappers_capture_the_inner_members() {
        #[derive(serde::Deserialize)]
        struct Wrapper(#[allow(dead_code)] ItemsQuery);
        assert_eq!(members::<Option<ItemsQuery>>(), members::<ItemsQuery>());
        assert_eq!(members::<Wrapper>(), members::<ItemsQuery>()); // `#[serde(transparent)]` (the HLS routes' query) forwards too.
        assert!(members::<crate::handlers::hls::HlsQueryPub>().contains(&"videoCodec"));
    }

    #[test]
    fn case_collisions_are_detected() {
        assert!(has_case_collision(&["parentId", "ParentID"]));
        assert!(!has_case_collision(&["parentId", "userId"]));
    }

    #[test]
    fn a_bad_value_is_still_axum_s_rejection() {
        let rejected = bind::<ItemsQuery>("/Items?islocked=maybe").expect_err("rejected");
        assert_eq!(
            axum::response::IntoResponse::into_response(rejected).status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn no_query_string_binds_defaults() {
        let query: ItemsQuery = bind("/Items").expect("binds");
        assert_eq!(query.is_locked, None);
    }
}
