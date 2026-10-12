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
//!   `{"MyKey":…,"mykey":…}` keeps both;
//! * enum **values** bind ignoring case too — `{"SubtitleMode":"onlyforced"}`
//!   is `OnlyForced` — because `JsonStringEnumConverter` reads names that way,
//!   as values and as dictionary keys (`{"ImageTags":{"primary":…}}`).
//!   [`Doc`]'s `deserialize_enum` folds a string against the enum's variant
//!   names (aliases included), exact match first.
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
//! Neither their member names nor their enum values fold (serde hands a
//! buffered string straight to the variant visitor), so a body DTO of either
//! shape binds through a flat wire struct instead — as `TimerInfoDto`,
//! `SeriesTimerInfoDto` (`ferrofin_model::live_tv`, `timer_info!`) and the
//! `RemoteSearchQuery` lookup infos (`ferrofin_model::providers`, `lookup_info!`)
//! do. Also unsupported, as no body uses it: a
//! `Box<serde_json::value::RawValue>` member.

use std::collections::{HashMap, HashSet};

use ferrofin_model::json::number::JsonNumber;
use serde::de::value::{MapAccessDeserializer, MapDeserializer, SeqDeserializer};
use serde::de::{self, IntoDeserializer, MapAccess, Visitor};
use serde::{Deserialize, forward_to_deserialize_any};
use serde_json::value::RawValue;

/// The error every [`Doc`] binding reports — `serde_json`'s, so messages read
/// exactly as they did when the binder deserialized a `serde_json::Value`.
type Error = serde_json::Error;

/// A parsed JSON request body whose objects keep their members in document
/// order (duplicates included), so a struct binding can resolve them the way
/// `System.Text.Json` does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Doc<const FOLD: bool = true> {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A validated number token, retaining its original spelling.
    Number(String),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Self>),
    /// An object, members in document order.
    Object(Vec<(String, Self)>),
}

impl<'de, const FOLD: bool> Deserialize<'de> for Doc<FOLD> {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <&RawValue>::deserialize(deserializer)?;
        Self::from_raw(raw.get(), 0).map_err(de::Error::custom)
    }
}

/// Reads an object's raw member values without sorting or collapsing its keys.
struct RawMembers;

impl<'de> Visitor<'de> for RawMembers {
    type Value = Vec<(String, &'de RawValue)>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut members = Vec::with_capacity(map.size_hint().unwrap_or_default());
        while let Some(member) = map.next_entry()? {
            members.push(member);
        }
        Ok(members)
    }
}

impl<const FOLD: bool> Doc<FOLD> {
    /// Builds the ordered tree while retaining numeric lexemes. Raw values
    /// borrow the request only during parsing; the completed tree owns its data.
    fn from_raw(raw: &str, depth: usize) -> Result<Self, Error> {
        if depth >= 128 {
            return Err(de::Error::custom("recursion limit exceeded"));
        }
        match raw.as_bytes()[0] {
            b'n' => Ok(Self::Null),
            b't' => Ok(Self::Bool(true)),
            b'f' => Ok(Self::Bool(false)),
            b'"' => serde_json::from_str(raw).map(Self::String),
            b'[' => serde_json::from_str::<Vec<&RawValue>>(raw)?
                .into_iter()
                .map(|value| Self::from_raw(value.get(), depth + 1))
                .collect::<Result<_, _>>()
                .map(Self::Array),
            b'{' => {
                use serde::Deserializer;
                let members =
                    serde_json::Deserializer::from_str(raw).deserialize_map(RawMembers)?;
                members
                    .into_iter()
                    .map(|(key, value)| Ok((key, Self::from_raw(value.get(), depth + 1)?)))
                    .collect::<Result<_, _>>()
                    .map(Self::Object)
            }
            _ => Ok(Self::Number(raw.to_owned())),
        }
    }

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
fn fold_members<const FOLD: bool>(
    members: Vec<(String, Doc<FOLD>)>,
    fields: &[&'static str],
) -> Vec<(String, Doc<FOLD>)> {
    let mut out: Vec<(String, Doc<FOLD>)> = Vec::with_capacity(members.len());
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

/// `name` as the entry of `names` it equals — exactly, else ignoring ASCII case
/// (the first such entry) — or unchanged when none matches, so the target's own
/// "unknown variant" error still names what the client sent.
fn fold_name(name: String, names: &[&'static str]) -> String {
    if names.contains(&name.as_str()) {
        return name;
    }
    names
        .iter()
        .find(|n| n.eq_ignore_ascii_case(&name))
        .map_or(name, |n| (*n).to_owned())
}

/// Collapses members whose names are EXACTLY equal to the last one, as the
/// sorted `serde_json::Value` map did and as `System.Text.Json`'s dictionary
/// converter (`dict[key] = value`) does. Names differing only in case stay
/// apart: on the map path they are distinct dictionary keys.
fn dedupe_exact<const FOLD: bool>(members: Vec<(String, Doc<FOLD>)>) -> Vec<(String, Doc<FOLD>)> {
    let unique = {
        let mut seen = HashSet::with_capacity(members.len());
        members.iter().all(|(name, _)| seen.insert(name.as_str()))
    };
    if unique {
        return members;
    }
    let mut out: Vec<(String, Doc<FOLD>)> = Vec::with_capacity(members.len());
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
fn visit_members<'de, V: Visitor<'de>, const FOLD: bool>(
    members: Vec<(String, Doc<FOLD>)>,
    visitor: V,
) -> Result<V::Value, Error> {
    let mut map = MapDeserializer::new(members.into_iter().map(|(name, value)| (Key(name), value)));
    let value = visitor.visit_map(&mut map)?;
    map.end()?;
    Ok(value)
}

impl<const FOLD: bool> IntoDeserializer<'_, Error> for Doc<FOLD> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

/// Numeric reads retain the target width and distinguish quoted overflow.
macro_rules! read_number {
    ($($method:ident => $visit:ident: $ty:ty),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            let (text, quoted) = match self {
                Self::String(text) => (text, true),
                Self::Number(text) => (text, false),
                other => return Err(de::Error::invalid_type(other.unexpected(), &visitor)),
            };
            match <$ty>::parse_json_number(&text, quoted) {
                Some(number) => visitor.$visit(number),
                None => Err(de::Error::invalid_value(de::Unexpected::Str(&text), &visitor)),
            }
        }
    )*};
}

impl<'de, const FOLD: bool> de::Deserializer<'de> for Doc<FOLD> {
    type Error = Error;

    read_number! {
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

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Self::Null => visitor.visit_unit(),
            Self::Bool(b) => visitor.visit_bool(b),
            Self::Number(n) => {
                if let Ok(value) = n.parse::<i64>() {
                    visitor.visit_i64(value)
                } else if let Ok(value) = n.parse::<u64>() {
                    visitor.visit_u64(value)
                } else {
                    visitor.visit_f64(n.parse().map_err(de::Error::custom)?)
                }
            }
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

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Self::String(text) | Self::Number(text) => visitor.visit_string(text),
            Self::Bool(value) => visitor.visit_string(value.to_string()),
            other => Err(de::Error::invalid_type(other.unexpected(), &visitor)),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Self::Object(members) => visit_members(
                if FOLD {
                    fold_members(members, fields)
                } else {
                    dedupe_exact(members)
                },
                visitor,
            ),
            other => Err(de::Error::invalid_type(
                other.unexpected(),
                &"a JSON object",
            )),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Self::String(variant) => {
                visitor.visit_enum(fold_name(variant, variants).into_deserializer())
            }
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
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        use ferrofin_model::json::value::{NULLABLE_VALUE, STRING_TOKEN};
        if name == ferrofin_model::json::number::PROFILE_I32 {
            let value = i32::deserialize(self)?;
            return visitor.visit_newtype_struct(value.into_deserializer());
        }
        if name == ferrofin_model::json::number::stored_f64::MARKER {
            return self.deserialize_f64(visitor);
        }
        match (name, self) {
            (NULLABLE_VALUE, Self::String(text)) if text.is_empty() => {
                visitor.visit_newtype_struct(Self::Null)
            }
            (STRING_TOKEN, value) if !matches!(value, Self::String(_)) => Err(
                de::Error::invalid_type(value.unexpected(), &"a JSON string token"),
            ),
            (_, value) => visitor.visit_newtype_struct(value),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        // An unknown member's subtree is skipped, not walked.
        visitor.visit_unit()
    }

    forward_to_deserialize_any! {
        bool char
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
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        // `EnumConverter.ReadAsPropertyNameCore` falls back to ignoring case
        // too, so `{"ImageTags":{"primary":…}}` binds `ImageType::Primary`.
        visitor.visit_enum(fold_name(self.0, variants).into_deserializer())
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
    fn scalars_bind_with_jellyfin_number_handling() {
        assert_eq!(bind::<i32>("-5").expect("i32"), -5);
        assert_eq!(bind::<u64>("18446744073709551615").expect("u64"), u64::MAX);
        assert!((bind::<f64>("1.5").expect("f64") - 1.5).abs() < f64::EPSILON);
        assert!(bind::<i32>("1.5").is_err());
        assert_eq!(bind::<i32>(r#""5""#).unwrap(), 5);
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
    fn enum_values_bind_ignoring_case() {
        for body in [r#""Plain""#, r#""plain""#, r#""PLAIN""#] {
            assert_eq!(bind::<Kind>(body).expect("unit"), Kind::Plain, "{body}");
        }
        let all: Vec<Option<Kind>> = bind(r#"["plain",null]"#).expect("nested");
        assert_eq!(all, [Some(Kind::Plain), None]);
        assert_eq!(
            bind::<Kind>(r#"{"Wrapped":3}"#).expect("newtype"),
            Kind::Wrapped(3)
        );
        let unknown = bind::<Kind>(r#""Nope""#).expect_err("unknown");
        assert!(unknown.to_string().contains("Nope"), "{unknown}");
        assert!(bind::<Kind>("3").is_err());
    }

    #[test]
    fn jellyfin_s_measured_user_configuration_body_binds() {
        // Live Jellyfin 12.2, POST /Users/{id}/Configuration.
        use ferrofin_model::configuration::{SubtitlePlaybackMode, UserConfiguration};
        let config: UserConfiguration =
            bind(r#"{"subtitlelanguagepreference":"eng","SUBTITLEMODE":"onlyforced"}"#)
                .expect("binds");
        assert_eq!(config.subtitle_language_preference.as_deref(), Some("eng"));
        assert_eq!(config.subtitle_mode, SubtitlePlaybackMode::OnlyForced);
    }

    #[test]
    fn the_formerly_flattened_bodies_bind_in_any_case() {
        use ferrofin_model::live_tv::{
            KeepUntil, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto,
        };
        use ferrofin_model::providers::{AlbumInfo, MovieInfo, RemoteSearchQuery};

        // POST /LiveTv/Timers: base members, own members and enum values.
        let timer: TimerInfoDto = bind(
            r#"{"name":"News","prePaddingSeconds":60,"keepUntil":"untilwatched","status":"inprogress","seriesTimerId":"s1"}"#,
        )
        .expect("timer");
        assert_eq!(timer.base.name.as_deref(), Some("News"));
        assert_eq!(timer.base.pre_padding_seconds, 60);
        assert_eq!(timer.base.keep_until, KeepUntil::UntilWatched);
        assert_eq!(timer.status, RecordingStatus::InProgress);
        assert_eq!(timer.series_timer_id.as_deref(), Some("s1"));

        // POST /LiveTv/SeriesTimers.
        let series: SeriesTimerInfoDto =
            bind(r#"{"CHANNELNAME":"BBC","recordanytime":true,"keepUpTo":2}"#).expect("series");
        assert_eq!(series.base.channel_name.as_deref(), Some("BBC"));
        assert!(series.record_any_time);
        assert_eq!(series.keep_up_to, 2);

        // POST /Items/RemoteSearch/Movie and /MusicAlbum (nested lookup infos).
        let movie: RemoteSearchQuery<MovieInfo> =
            bind(r#"{"searchInfo":{"name":"Alien","year":1979},"includeDisabledProviders":true}"#)
                .expect("movie");
        let info = movie.search_info.expect("search info");
        assert_eq!(info.base.name.as_deref(), Some("Alien"));
        assert_eq!(info.base.year, Some(1979));
        assert!(movie.include_disabled_providers);
        let album: RemoteSearchQuery<AlbumInfo> = bind(
            r#"{"searchInfo":{"name":"X","albumArtists":["A"],"songInfos":[{"name":"S","album":"X"}]}}"#,
        )
        .expect("album");
        let info = album.search_info.expect("search info");
        assert_eq!(info.album_artists, ["A"]);
        assert_eq!(info.song_infos[0].base.name.as_deref(), Some("S"));
        assert_eq!(info.song_infos[0].album.as_deref(), Some("X"));
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
        for body in [r#"{"Plain":1}"#, r#"{"plain":1}"#] {
            let kinds: HashMap<Kind, i32> = bind(body).expect("enum keys");
            assert_eq!(kinds[&Kind::Plain], 1, "{body}");
        }
        // String keys are data, never folded.
        let names: HashMap<String, i32> = bind(r#"{"plain":1}"#).expect("string keys");
        assert!(names.contains_key("plain"));
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
        assert!(bind::<Outer>(r#"{"Name":{}}"#).is_err());
    }
    #[test]
    #[allow(clippy::float_cmp)] // Parsing must match the oracle exactly.
    fn numeric_body_binding_matches_every_oracle_case() {
        let fixture = include_str!("../../tests/data/json-binding/jellyfin-12.2.jsonl");
        let mut checked = 0;
        for line in fixture.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = row["input"].as_str().unwrap();
            macro_rules! check {
                ($ty:ty) => {{
                    let actual = bind::<$ty>(input);
                    assert_eq!(
                        actual.is_ok(),
                        row["accepted"].as_bool().unwrap(),
                        "{} from {input}: {actual:?}",
                        stringify!($ty)
                    );
                    if let Ok(actual) = actual {
                        let expected = row["value"].as_str().unwrap();
                        if expected == "NaN" {
                            assert_eq!(actual.to_string(), "NaN");
                        } else {
                            let expected: $ty = expected.parse().unwrap();
                            assert_eq!(actual, expected, "{} from {input}", stringify!($ty));
                        }
                    }
                    checked += 1;
                }};
            }
            match row["type"].as_str().unwrap() {
                "i8" => check!(i8),
                "i16" => check!(i16),
                "i32" => check!(i32),
                "i64" => check!(i64),
                "u8" => check!(u8),
                "u16" => check!(u16),
                "u32" => check!(u32),
                "u64" => check!(u64),
                "f32" => check!(f32),
                "f64" => check!(f64),
                _ => {}
            }
        }
        assert_eq!(checked, 1090);
    }

    #[test]
    fn raw_numbers_preserve_lexemes_and_depth_is_bounded() {
        for raw in ["1.50", "1E+02", "-0", "18446744073709551616", "1e400"] {
            assert_eq!(
                serde_json::from_str::<Doc>(raw).unwrap(),
                Doc::Number(raw.into())
            );
        }
        let nested = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        assert!(serde_json::from_str::<Doc>(&nested).is_err());
        assert!(serde_json::from_str::<Doc>("[1,]").is_err());
    }
    #[test]
    fn string_and_boolean_binding_match_jellyfin_oracle() {
        let fixture = include_str!("../../tests/data/json-binding/jellyfin-12.2.jsonl");
        for line in fixture.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = row["input"].as_str().unwrap();
            let accepted = row["accepted"].as_bool().unwrap();
            match row["type"].as_str().unwrap() {
                "string" => {
                    let actual = bind::<Option<String>>(input);
                    assert_eq!(actual.is_ok(), accepted, "string from {input}: {actual:?}");
                    if let Ok(value) = actual {
                        assert_eq!(value.as_deref(), row["value"].as_str(), "{input}");
                    }
                }
                "bool" => assert_eq!(bind::<bool>(input).is_ok(), accepted, "bool from {input}"),
                _ => {}
            }
        }
    }

    #[test]
    fn strings_inside_lists_and_dictionaries_preserve_raw_numbers() {
        let list: Vec<String> = bind("[1.50,-0,1E+02,true,false]").unwrap();
        assert_eq!(list, ["1.50", "-0", "1E+02", "true", "false"]);
        let map: HashMap<String, Option<String>> = bind(r#"{"a":1.50,"A":null}"#).unwrap();
        assert_eq!(map["a"].as_deref(), Some("1.50"));
        assert_eq!(map["A"], None);
        assert!(bind::<Vec<String>>("[{}]").is_err());
        assert!(bind::<Vec<String>>("[[]]").is_err());
    }
    #[test]
    fn numeric_enums_keep_their_value_and_integer_lexical_rules() {
        use ferrofin_model::configuration::SubtitlePlaybackMode as Mode;
        use ferrofin_model::data::MediaStreamProtocol;
        for input in ["3", r#""3""#, r#"" 3 ""#, r#""onlyforced,Always""#] {
            assert_eq!(bind::<Mode>(input).unwrap(), Mode::None);
        }
        assert_eq!(bind::<Mode>("-0").unwrap(), Mode::Default);
        assert_eq!(bind::<Mode>("999").unwrap(), Mode::Unrecognized(999));
        assert_eq!(bind::<Mode>(r#""-1""#).unwrap(), Mode::Unrecognized(-1));
        for bad in [
            "3.0",
            "3e0",
            "-0.0",
            "true",
            "null",
            r#""""#,
            r#""no-such-mode""#,
        ] {
            assert!(bind::<Mode>(bad).is_err(), "{bad}");
        }
        for default in ["null", r#""""#] {
            assert_eq!(
                bind::<MediaStreamProtocol>(default).unwrap(),
                MediaStreamProtocol::http
            );
        }
    }
    #[derive(Debug, Deserialize)]
    #[serde(transparent)]
    struct Guid(#[serde(with = "ferrofin_model::json::guid")] uuid::Uuid);
    #[derive(Debug, Deserialize)]
    #[serde(transparent)]
    struct OptionalGuid(#[serde(with = "ferrofin_model::json::guid::option")] Option<uuid::Uuid>);
    #[derive(Debug, Deserialize)]
    #[serde(transparent)]
    struct Date(#[serde(with = "ferrofin_model::json::datetime")] chrono::DateTime<chrono::Utc>);
    #[derive(Debug, Deserialize)]
    #[serde(transparent)]
    struct OptionalDate(
        #[serde(with = "ferrofin_model::json::datetime::option")]
        Option<chrono::DateTime<chrono::Utc>>,
    );
    #[derive(Debug, Deserialize)]
    #[serde(transparent)]
    struct Nullable<T>(
        #[serde(
            deserialize_with = "ferrofin_model::json::value::nullable",
            bound(deserialize = "T: Deserialize<'de>")
        )]
        Option<T>,
    );

    #[derive(Debug, PartialEq)]
    struct OracleEnum(i32);
    impl ferrofin_model::json::enums::JsonEnum for OracleEnum {
        fn from_discriminant(value: i32) -> Self {
            Self(value)
        }
        fn members() -> &'static [(&'static str, i32)] {
            &[("First", 1), ("Second", 4)]
        }
        fn json_default() -> Option<i32> {
            None
        }
    }
    impl<'de> Deserialize<'de> for OracleEnum {
        fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            ferrofin_model::json::enums::deserialize(d)
        }
    }

    #[test]
    fn nullable_guid_and_date_binding_match_jellyfin_oracle() {
        let fixture = include_str!("../../tests/data/json-binding/jellyfin-12.2.jsonl");
        let edges = include_str!("../../tests/data/json-binding/jellyfin-12.2-dates.jsonl");
        let mut checked = 0;
        for line in fixture.lines().chain(edges.lines()) {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = row["input"].as_str().unwrap();
            let kind = row["type"].as_str().unwrap();
            let actual: Result<Option<String>, Error> = match kind {
                "i32?" => bind::<Nullable<i32>>(input).map(|n| n.0.map(|v| v.to_string())),
                "bool?" => bind::<Nullable<bool>>(input)
                    .map(|n| n.0.map(|v| if v { "True" } else { "False" }.to_owned())),
                "enum?" => bind::<Nullable<OracleEnum>>(input).map(|n| {
                    n.0.map(|v| match v.0 {
                        1 => "First".into(),
                        4 => "Second".into(),
                        n => n.to_string(),
                    })
                }),
                "guid" => bind::<Guid>(input).map(|g| Some(g.0.to_string())),
                "guid?" => bind::<OptionalGuid>(input).map(|g| g.0.map(|v| v.to_string())),
                "date" => bind::<Date>(input).map(|d| Some(d.0.to_rfc3339())),
                "date?" => bind::<OptionalDate>(input).map(|d| d.0.map(|v| v.to_rfc3339())),
                _ => continue,
            };
            assert_eq!(
                actual.is_ok(),
                row["accepted"].as_bool().unwrap(),
                "{kind} from {input}: {actual:?}"
            );
            if let Ok(value) = actual {
                let expected = row["value"].as_str().map(|text| {
                    if kind.starts_with("date") {
                        ferrofin_model::json::datetime::parse(text)
                            .unwrap()
                            .to_rfc3339()
                    } else {
                        text.to_owned()
                    }
                });
                assert_eq!(value, expected, "{kind} from {input}");
            }
            checked += 1;
        }
        assert_eq!(checked, 815);
    }

    #[test]
    fn nullable_markers_apply_to_value_fields_only() {
        use ferrofin_model::tasks::TaskTriggerInfo;
        #[derive(Deserialize)]
        struct Ids {
            #[serde(with = "ferrofin_model::json::guid::vec")]
            ids: Vec<uuid::Uuid>,
        }
        let task: TaskTriggerInfo =
            bind(r#"{"Type":0,"IntervalTicks":"","DayOfWeek":""}"#).unwrap();
        assert_eq!(task.interval_ticks, None);
        assert_eq!(task.day_of_week, None);
        assert!(bind::<TaskTriggerInfo>(r#"{"Type":0,"IntervalTicks":" "}"#).is_err());
        assert_eq!(
            bind::<Option<String>>(r#""""#).unwrap().as_deref(),
            Some("")
        );
        assert!(bind::<Option<Vec<i32>>>(r#""""#).is_err());
        assert!(bind::<Guid>("12345678901234567890123456789012").is_err());
        assert!(bind::<Guid>(r#""urn:uuid:00000000-0000-0000-0000-000000000001""#).is_err());
        let ids: Ids = bind(r#"{"ids":[null,"(00000000-0000-0000-0000-000000000001)"]}"#).unwrap();
        assert_eq!(ids.ids, [uuid::Uuid::nil(), uuid::Uuid::from_u128(1)]);
        assert!(bind::<Ids>(r#"{"ids":[""]}"#).is_err());
    }
    #[test]
    fn stored_floating_point_fields_keep_typed_negative_zero_and_overflow_rules() {
        use ferrofin_model::configuration::EncodingOptions;
        for input in [
            r#"{"DownMixAudioBoost":-0}"#,
            r#"{"DownMixAudioBoost":"-0"}"#,
        ] {
            let options: EncodingOptions = bind(input).unwrap();
            assert_eq!(options.down_mix_audio_boost.to_bits(), (-0.0_f64).to_bits());
        }
        let options: EncodingOptions = bind(r#"{"DownMixAudioBoost":1e999}"#).unwrap();
        assert!(options.down_mix_audio_boost.is_infinite());
        assert!(options.down_mix_audio_boost.is_sign_positive());
        assert!(bind::<EncodingOptions>(r#"{"DownMixAudioBoost":"1e999"}"#).is_err());
    }

    #[test]
    fn exact_property_mode_shares_values_and_recurses_without_folding() {
        #[derive(Debug, Default, Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Config {
            count: i32,
            name: String,
            #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
            optional: Option<bool>,
            child: Option<Box<Config>>,
            values: HashMap<String, String>,
        }
        let body = r#"{"Count":"3","count":"999","Name":1.50,"Optional":"","Child":{"Count":"4","count":"bad"},"Values":{"A":1E+02,"a":true}}"#;
        let result: Config = crate::extract::deserialize_defaults(body).unwrap();
        assert_eq!(result.count, 3);
        assert_eq!(result.name, "1.50");
        assert_eq!(result.optional, None);
        assert_eq!(result.child.unwrap().count, 4);
        assert_eq!(result.values["A"], "1E+02");
        assert_eq!(result.values["a"], "true");
        let duplicate: Config =
            crate::extract::deserialize_defaults(r#"{"Count":1,"Count":"2"}"#).unwrap();
        assert_eq!(duplicate.count, 2);
        assert!(crate::extract::deserialize_defaults::<Config>(r#"{"Child":[]}"#).is_err());
        assert!(crate::extract::deserialize_defaults::<Config>(r#"{"Optional":"true"}"#).is_err());
    }
    #[test]
    fn profile_file_leniency_does_not_override_request_number_rules() {
        use ferrofin_model::dlna::TranscodingProfile;
        let file: TranscodingProfile =
            serde_json::from_str(r#"{"MinSegments":"","SegmentLength":"2"}"#).unwrap();
        assert_eq!(file.min_segments, 0);
        assert_eq!(file.segment_length, 2);
        let body: TranscodingProfile = bind(r#"{"MinSegments":"2","Protocol":"1"}"#).unwrap();
        assert_eq!(body.min_segments, 2);
        assert_eq!(
            body.protocol,
            ferrofin_model::data::MediaStreamProtocol::hls
        );
        for input in [
            r#"{"MinSegments":""}"#,
            r#"{"MinSegments":" 2"}"#,
            r#"{"MinSegments":2.0}"#,
        ] {
            assert!(bind::<TranscodingProfile>(input).is_err(), "{input}");
        }
        let stream: ferrofin_model::entities_media::MediaStream =
            bind(r#"{"Type":"999"}"#).unwrap();
        assert_eq!(
            stream.stream_type,
            ferrofin_model::entities::MediaStreamType::Unrecognized(999)
        );
    }

    #[test]
    fn flat_timer_and_lookup_wires_retain_numeric_and_nullable_rules() {
        use ferrofin_model::{
            live_tv::{SeriesTimerInfoDto, TimerInfoDto},
            providers::{MovieInfo, RemoteSearchQuery},
        };
        let timer: TimerInfoDto =
            bind(r#"{"prePaddingSeconds":"60","runTimeTicks":"","keepUntil":1}"#).unwrap();
        assert_eq!(timer.base.pre_padding_seconds, 60);
        assert_eq!(timer.run_time_ticks, None);
        let series: SeriesTimerInfoDto = bind(r#"{"keepUpTo":"2","dayPattern":""}"#).unwrap();
        assert_eq!(series.keep_up_to, 2);
        assert_eq!(series.day_pattern, None);
        let movie: RemoteSearchQuery<MovieInfo> =
            bind(r#"{"SearchInfo":{"year":"1979","IndexNumber":""}}"#).unwrap();
        let info = movie.search_info.unwrap();
        assert_eq!(info.base.year, Some(1979));
        assert_eq!(info.base.index_number, None);
    }
}
