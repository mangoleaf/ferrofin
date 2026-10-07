//! The request-body document the binder deserializes from: a JSON tree that
//! keeps object members in **document order** and binds struct members
//! **ignoring ASCII case** — the port of the `PropertyNameCaseInsensitive = true`
//! that ASP.NET's MVC JSON options inherit from `JsonSerializerDefaults.Web`.
//!
//! Jellyfin's `AddJsonOptions` (v12.2 Jellyfin.Server/Extensions/
//! ApiServiceCollectionExtensions.cs:137-156) copies everything else from
//! `JsonDefaults.PascalCaseOptions` but never that flag, so every `[FromBody]`
//! DTO binds `{"updates":[{"path":…}]}` exactly like `{"Updates":[{"Path":…}]}`
//! — which is what Sonarr and Radarr send (Newtonsoft's
//! `CamelCasePropertyNamesContractResolver`). Measured on a live Jellyfin 12.2.0:
//!
//! * camelCase, lowercase and UPPERCASE member names all bind, at every depth;
//! * two members differing only in case bind the one **last in the document**
//!   — `{"A":"first","a":"second"}` and `{"a":"first","A":"second"}` both give
//!   `second` — with no error;
//! * dictionary keys (`DisplayPreferences.CustomPrefs`) are never folded:
//!   `{"MyKey":…,"mykey":…}` keeps both.
//!
//! The fold therefore lives in [`Doc`]'s `deserialize_struct`, the one place a
//! serde-derived struct hands over its member names (`FIELDS`, aliases
//! included). A map target asks for `deserialize_map` instead and sees its keys
//! verbatim. A `serde_json::Value` cannot carry this: without the workspace-wide
//! `preserve_order` feature its objects are sorted, so "last in the document" is
//! lost, and that feature would reorder every `Value` the server serializes.
//!
//! Everywhere else a [`Doc`] binds exactly as the `serde_json::Value` it
//! replaced: exact duplicate names collapse to the last one on the map path too,
//! map keys parse into integer, bool, newtype and enum keys, and a data-carrying
//! enum still needs a single-member object.
//!
//! Known blind spots — targets serde routes through its private `Content`
//! buffer, which never reaches [`Doc`]'s `deserialize_struct`:
//!
//! * a struct with a `#[serde(flatten)]` member (it asks for `deserialize_map`);
//! * `#[serde(untagged)]` and internally tagged (`tag = "…"`) enums whose
//!   variants carry members;
//! * the struct variants of an externally tagged enum (serde asks for
//!   `deserialize_map`), which also accept an array as `Value` did not.
//!
//! None of their member names fold, so a body DTO of either shape binds through
//! a flat wire struct instead. TODO: `TimerInfoDto`, `SeriesTimerInfoDto` and the
//! `RemoteSearchQuery` lookup infos still flatten (`POST /LiveTv/Timers*`,
//! `POST /LiveTv/SeriesTimers*`, `POST /Items/RemoteSearch/*`); their wire
//! structs are execution step 2 (D3) of `PLAN_JSON_BODY_CASE_INSENSITIVE.md`. Also unsupported,
//! as no body uses it: a `Box<serde_json::value::RawValue>` member.

use std::collections::{HashMap, HashSet};

use serde::de::value::{MapAccessDeserializer, MapDeserializer, SeqDeserializer};
use serde::de::{self, IntoDeserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, forward_to_deserialize_any};
use serde_json::Number;

/// The error every [`Doc`] binding reports — `serde_json`'s, so messages read
/// exactly as they did when the binder deserialized a `serde_json::Value`.
type Error = serde_json::Error;

/// A parsed JSON request body whose objects keep their members in document
/// order (duplicates included), so a struct binding can resolve them the way
/// `System.Text.Json` does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Doc {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number, as `serde_json` parsed it.
    Number(Number),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Doc>),
    /// An object, members in document order.
    Object(Vec<(String, Doc)>),
}

impl<'de> Deserialize<'de> for Doc {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(DocVisitor)
    }
}

/// Builds a [`Doc`] from any self-describing format (in practice `serde_json`).
struct DocVisitor;

impl<'de> Visitor<'de> for DocVisitor {
    type Value = Doc;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_unit<E>(self) -> Result<Doc, E> {
        Ok(Doc::Null)
    }

    fn visit_none<E>(self) -> Result<Doc, E> {
        Ok(Doc::Null)
    }

    fn visit_some<D: de::Deserializer<'de>>(self, d: D) -> Result<Doc, D::Error> {
        Doc::deserialize(d)
    }

    fn visit_bool<E>(self, b: bool) -> Result<Doc, E> {
        Ok(Doc::Bool(b))
    }

    fn visit_i64<E>(self, n: i64) -> Result<Doc, E> {
        Ok(Doc::Number(n.into()))
    }

    fn visit_u64<E>(self, n: u64) -> Result<Doc, E> {
        Ok(Doc::Number(n.into()))
    }

    fn visit_f64<E: de::Error>(self, n: f64) -> Result<Doc, E> {
        // `serde_json` never yields a non-finite float from JSON text.
        Number::from_f64(n)
            .map(Doc::Number)
            .ok_or_else(|| E::custom("a JSON number must be finite"))
    }

    fn visit_str<E>(self, s: &str) -> Result<Doc, E> {
        Ok(Doc::String(s.to_owned()))
    }

    fn visit_string<E>(self, s: String) -> Result<Doc, E> {
        Ok(Doc::String(s))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Doc, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or_default());
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Doc::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Doc, A::Error> {
        let mut members = Vec::with_capacity(map.size_hint().unwrap_or_default());
        while let Some(member) = map.next_entry()? {
            members.push(member);
        }
        Ok(Doc::Object(members))
    }
}

impl Doc {
    /// Whether this is an object — the top-level kind every struct DTO needs.
    pub(crate) fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }

    /// Whether this is an array — the top-level kind of a list body.
    pub(crate) fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
    }

    /// Whether this is `null`.
    pub(crate) fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// The `serde` description of this value, for a type-mismatch error.
    fn unexpected(&self) -> de::Unexpected<'_> {
        match self {
            Self::Null => de::Unexpected::Unit,
            Self::Bool(b) => de::Unexpected::Bool(*b),
            Self::Number(_) => de::Unexpected::Other("number"),
            Self::String(s) => de::Unexpected::Str(s),
            Self::Array(_) => de::Unexpected::Seq,
            Self::Object(_) => de::Unexpected::Map,
        }
    }
}

/// Canonicalises an object's member names against a struct's `fields`, then
/// keeps one value per field — the LAST in document order, `System.Text.Json`'s
/// rule for duplicates (exact or case-variant).
///
/// A name matching a field exactly keeps it; otherwise the first field equal to
/// it ignoring ASCII case is taken (aliases sit in `fields` too, so a case
/// variant of an alias binds the alias). Members matching no field pass through
/// unchanged, where the struct ignores them exactly as before.
///
/// ponytail: linear scans over `fields` per member, O(members × fields); fine at
/// Jellyfin's DTO sizes (`BaseItemDto` ≈ 150). A per-struct name index is the
/// upgrade if a body ever measures slow.
fn fold_members(members: Vec<(String, Doc)>, fields: &[&'static str]) -> Vec<(String, Doc)> {
    let mut out: Vec<(String, Doc)> = Vec::with_capacity(members.len());
    // `out` position of each field already bound, indexed like `fields`.
    let mut bound: Vec<Option<usize>> = vec![None; fields.len()];
    for (name, value) in members {
        // The slot is the FIRST name equal ignoring case, so an alias that
        // differs from its field only by case (`EnableIPv4`/`EnableIpv4`)
        // shares the field's slot instead of tripping serde's duplicate check.
        let Some(i) = fields.iter().position(|f| f.eq_ignore_ascii_case(&name)) else {
            out.push((name, value));
            continue;
        };
        // Reuse the member's own name when the struct already knows it.
        let name = if fields.contains(&name.as_str()) {
            name
        } else {
            fields[i].to_owned()
        };
        if let Some(at) = bound[i] {
            out[at].1 = value;
        } else {
            bound[i] = Some(out.len());
            out.push((name, value));
        }
    }
    out
}

/// Collapses members whose names are EXACTLY equal to the last one, as the
/// sorted `serde_json::Value` map did and as `System.Text.Json`'s dictionary
/// converter (`dict[key] = value`) does. Names differing only in case stay
/// apart: on the map path they are distinct dictionary keys.
fn dedupe_exact(members: Vec<(String, Doc)>) -> Vec<(String, Doc)> {
    let unique = {
        let mut seen = HashSet::with_capacity(members.len());
        members.iter().all(|(name, _)| seen.insert(name.as_str()))
    };
    if unique {
        return members;
    }
    let mut out: Vec<(String, Doc)> = Vec::with_capacity(members.len());
    let mut at: HashMap<String, usize> = HashMap::with_capacity(members.len());
    for (name, value) in members {
        if let Some(&i) = at.get(&name) {
            out[i].1 = value;
        } else {
            at.insert(name.clone(), out.len());
            out.push((name, value));
        }
    }
    out
}

/// Feeds an object's members to `visitor` as a map, each value still a [`Doc`]
/// so nested structs fold too. Callers dedupe first.
fn visit_members<'de, V: Visitor<'de>>(
    members: Vec<(String, Doc)>,
    visitor: V,
) -> Result<V::Value, Error> {
    let mut map = MapDeserializer::new(members.into_iter().map(|(name, value)| (Key(name), value)));
    let value = visitor.visit_map(&mut map)?;
    map.end()?;
    Ok(value)
}

impl IntoDeserializer<'_, Error> for Doc {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

impl<'de> de::Deserializer<'de> for Doc {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Self::Null => visitor.visit_unit(),
            Self::Bool(b) => visitor.visit_bool(b),
            Self::Number(n) => de::Deserializer::deserialize_any(n, visitor),
            Self::String(s) => visitor.visit_string(s),
            Self::Array(items) => {
                let mut seq = SeqDeserializer::new(items.into_iter());
                let value = visitor.visit_seq(&mut seq)?;
                seq.end()?;
                Ok(value)
            }
            Self::Object(members) => visit_members(dedupe_exact(members), visitor),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Self::Object(members) => visit_members(fold_members(members, fields), visitor),
            other => other.deserialize_any(visitor),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Self::String(variant) => visitor.visit_enum(variant.into_deserializer()),
            // An externally tagged data variant: `{"Variant": value}`, exactly
            // one member, as `serde_json::Value` demands.
            Self::Object(members) if members.len() == 1 => MapAccessDeserializer::new(
                MapDeserializer::new(members.into_iter().map(|(name, value)| (Key(name), value))),
            )
            .deserialize_enum(name, variants, visitor),
            Self::Object(_) => Err(de::Error::invalid_value(
                de::Unexpected::Map,
                &"map with a single key",
            )),
            other => Err(de::Error::invalid_type(
                other.unexpected(),
                &"a string or a single-member object",
            )),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Self::Null => visitor.visit_none(),
            other => visitor.visit_some(other),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        // An unknown member's subtree is skipped, not walked.
        visitor.visit_unit()
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map identifier
    }
}

/// An object member name in key position — the port of `serde_json`'s map-key
/// deserializer, so a dictionary keyed by an integer, a bool, a newtype or an
/// enum binds from its JSON string key exactly as it did from a `Value`.
struct Key(String);

impl IntoDeserializer<'_, Error> for Key {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

/// `deserialize_<scalar>` for [`Key`]: the name must itself be that JSON
/// literal (`"5"`, `"-1"`, `"true"`; not `"+5"`, `"05"` or `"NaN"`), as
/// `serde_json`'s map-key deserializer requires.
macro_rules! parse_key {
    ($($method:ident => $visit:ident: $ty:ty),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            match serde_json::from_str::<$ty>(&self.0) {
                Ok(n) => visitor.$visit(n),
                Err(_) => Err(de::Error::invalid_type(de::Unexpected::Str(&self.0), &visitor)),
            }
        }
    )*};
}

impl<'de> de::Deserializer<'de> for Key {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_string(self.0)
    }

    parse_key! {
        deserialize_bool => visit_bool: bool,
        deserialize_i8 => visit_i8: i8,
        deserialize_i16 => visit_i16: i16,
        deserialize_i32 => visit_i32: i32,
        deserialize_i64 => visit_i64: i64,
        deserialize_i128 => visit_i128: i128,
        deserialize_u8 => visit_u8: u8,
        deserialize_u16 => visit_u16: u16,
        deserialize_u32 => visit_u32: u32,
        deserialize_u64 => visit_u64: u64,
        deserialize_u128 => visit_u128: u128,
        deserialize_f32 => visit_f32: f32,
        deserialize_f64 => visit_f64: f64,
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_enum(self.0.into_deserializer())
    }

    forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct
        map struct identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn bind<T: de::DeserializeOwned>(json: &str) -> Result<T, Error> {
        T::deserialize(serde_json::from_str::<Doc>(json).expect("parses"))
    }

    #[derive(Debug, Default, PartialEq, serde::Deserialize)]
    #[serde(rename_all = "PascalCase", default)]
    struct Inner {
        path: Option<String>,
        update_type: Option<String>,
    }

    #[derive(Debug, Default, PartialEq, serde::Deserialize)]
    #[serde(rename_all = "PascalCase", default)]
    struct Outer {
        updates: Vec<Inner>,
        name: Option<String>,
        #[serde(alias = "IsHD")]
        is_hd: Option<bool>,
        custom_prefs: HashMap<String, String>,
        child: Option<Box<Outer>>,
    }

    #[test]
    fn sonarr_and_radarr_camel_case_bodies_bind() {
        // Sonarr v5-develop / Radarr develop `MediaBrowserProxy.Update`:
        // Newtonsoft camelCase, indented, nulls omitted.
        let body = "{\n  \"updates\": [\n    {\n      \"path\": \"/tv/Show\",\n      \"updateType\": \"Created\"\n    }\n  ]\n}";
        let dto: Outer = bind(body).expect("binds");
        assert_eq!(
            dto.updates,
            [Inner {
                path: Some("/tv/Show".into()),
                update_type: Some("Created".into()),
            }]
        );
    }

    #[test]
    fn every_casing_binds_at_every_depth() {
        for body in [
            r#"{"Name":"x","Child":{"Name":"y","Updates":[{"Path":"p"}]}}"#,
            r#"{"name":"x","child":{"name":"y","updates":[{"path":"p"}]}}"#,
            r#"{"NAME":"x","CHILD":{"NAME":"y","UPDATES":[{"PATH":"p"}]}}"#,
        ] {
            let dto: Outer = bind(body).expect("binds");
            assert_eq!(dto.name.as_deref(), Some("x"), "{body}");
            let child = dto.child.expect("child");
            assert_eq!(child.name.as_deref(), Some("y"), "{body}");
            assert_eq!(child.updates[0].path.as_deref(), Some("p"), "{body}");
        }
    }

    #[test]
    fn the_last_duplicate_in_document_order_wins() {
        // Jellyfin 12.2: `second` for both orders, no error.
        for body in [
            r#"{"Name":"first","name":"second"}"#,
            r#"{"name":"first","Name":"second"}"#,
            r#"{"Name":"first","Name":"second"}"#,
        ] {
            let dto: Outer = bind(body).expect("binds");
            assert_eq!(dto.name.as_deref(), Some("second"), "{body}");
        }
    }

    #[test]
    fn dictionary_keys_are_never_folded() {
        let dto: Outer = bind(r#"{"customPrefs":{"MyKey":"a","mykey":"b"}}"#).expect("binds");
        assert_eq!(dto.custom_prefs.len(), 2);
        assert_eq!(dto.custom_prefs["MyKey"], "a");
        assert_eq!(dto.custom_prefs["mykey"], "b");
    }

    #[test]
    fn aliases_fold_too() {
        for body in [r#"{"IsHd":true}"#, r#"{"IsHD":true}"#, r#"{"ishd":true}"#] {
            let dto: Outer = bind(body).expect("binds");
            assert_eq!(dto.is_hd, Some(true), "{body}");
        }
    }

    #[derive(Debug, Default, serde::Deserialize)]
    #[serde(default)]
    struct CaseAlias {
        #[serde(rename = "EnableIPv4", alias = "EnableIpv4")]
        enable_ipv4: bool,
    }

    #[test]
    fn a_case_only_alias_shares_its_fields_slot() {
        let dto: CaseAlias = bind(r#"{"enableipv4":true,"EnableIpv4":false}"#).expect("binds");
        assert!(!dto.enable_ipv4);
        let dto: CaseAlias = bind(r#"{"EnableIPv4":false,"enableipv4":true}"#).expect("binds");
        assert!(dto.enable_ipv4);
    }

    #[test]
    fn unknown_members_are_ignored_as_before() {
        let dto: Outer = bind(r#"{"Nope":1,"name":"x"}"#).expect("binds");
        assert_eq!(dto.name.as_deref(), Some("x"));
    }

    #[test]
    fn scalars_bind_as_serde_json_values_did() {
        assert_eq!(bind::<i32>("-5").expect("i32"), -5);
        assert_eq!(bind::<u64>("18446744073709551615").expect("u64"), u64::MAX);
        assert!((bind::<f64>("1.5").expect("f64") - 1.5).abs() < f64::EPSILON);
        assert!(bind::<i32>("1.5").is_err());
        assert!(bind::<i32>(r#""5""#).is_err());
        assert!(bind::<bool>(r#""true""#).is_err());
        assert_eq!(bind::<Option<i32>>("null").expect("none"), None);
        assert_eq!(bind::<()>("null").expect("unit"), ());
        assert_eq!(
            bind::<serde_json::Value>(r#"{"b":[1,"x",null,true]}"#).expect("value"),
            serde_json::json!({"b":[1,"x",null,true]})
        );
    }

    #[derive(Debug, PartialEq, Eq, Hash, serde::Deserialize)]
    enum Kind {
        Plain,
        Wrapped(i32),
    }

    #[test]
    fn enums_bind_by_exact_name() {
        assert_eq!(bind::<Kind>(r#""Plain""#).expect("unit"), Kind::Plain);
        assert_eq!(
            bind::<Kind>(r#"{"Wrapped":3}"#).expect("newtype"),
            Kind::Wrapped(3)
        );
        assert!(bind::<Kind>(r#""Nope""#).is_err());
        assert!(bind::<Kind>("3").is_err());
    }

    #[test]
    fn a_data_variant_needs_exactly_one_member() {
        assert!(bind::<Kind>(r#"{"Wrapped":3,"Extra":4}"#).is_err());
        assert!(bind::<Kind>("{}").is_err());
    }

    #[test]
    fn exact_duplicates_collapse_to_the_last_on_the_map_path() {
        let value: serde_json::Value = bind(r#"{"A":1,"a":2,"A":3}"#).expect("value");
        assert_eq!(value, serde_json::json!({"A":3,"a":2}));
        let map: HashMap<String, i32> = bind(r#"{"k":1,"k":2}"#).expect("map");
        assert_eq!(map["k"], 2);
        let flat: Flat = bind(r#"{"Name":"a","Name":"b","Extra":1,"Extra":2}"#).expect("flatten");
        assert_eq!(flat.base.name.as_deref(), Some("b"));
        assert_eq!(flat.extra, 2);
    }

    #[derive(Debug, Default, serde::Deserialize)]
    #[serde(rename_all = "PascalCase", default)]
    struct Flat {
        #[serde(flatten)]
        base: Outer,
        extra: i32,
    }

    #[test]
    fn map_keys_parse_as_serde_json_keys_did() {
        let ints: HashMap<i32, String> = bind(r#"{"5":"a","-1":"b"}"#).expect("int keys");
        assert_eq!(ints[&5], "a");
        assert_eq!(ints[&-1], "b");
        let bools: HashMap<bool, i32> = bind(r#"{"true":1}"#).expect("bool keys");
        assert_eq!(bools[&true], 1);
        let kinds: HashMap<Kind, i32> = bind(r#"{"Plain":1}"#).expect("enum keys");
        assert_eq!(kinds[&Kind::Plain], 1);
        for bad in [r#"{"x":1}"#, r#"{"+5":1}"#, r#"{"05":1}"#] {
            assert!(bind::<HashMap<i32, i32>>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn null_members_and_empty_objects_bind() {
        let dto: Outer = bind(r#"{"name":null,"child":null}"#).expect("nulls");
        assert_eq!(dto, Outer::default());
        assert_eq!(bind::<Outer>("{}").expect("empty"), Outer::default());
    }

    #[test]
    fn a_type_mismatch_is_still_an_error() {
        assert!(bind::<Outer>(r#"{"Updates":"nope"}"#).is_err());
        assert!(bind::<Outer>(r#"{"Name":5}"#).is_err());
    }
}
