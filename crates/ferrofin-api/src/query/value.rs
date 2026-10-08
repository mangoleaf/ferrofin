//! Query values bound the way ASP.NET's `SimpleTypeModelBinder` binds them.
//!
//! A struct DTO's query values reach serde through [`Pairs`] instead of
//! `serde_urlencoded`'s own deserializer, which parses each value with Rust's
//! strict `FromStr`. ASP.NET converts the string with the target type's
//! `TypeConverter`, after `ConvertEmptyStringToNull`:
//!
//! * an empty or whitespace-only value binds a nullable member to `null`
//!   (`?isFavorite=`, `?searchTerm=`), where serde saw `Some("")` or failed —
//!   and is a 400 for a member whose C# type is a NON-nullable value type
//!   (`bool enableTotalRecordCount = true`), as `CheckModel` answers "The value
//!   '' is invalid." — modelled by the DTO or not. Ferrofin models those as
//!   `Option` + a default, so the generated per-action table
//!   (`non_nullable.json`, see `contracts/gen_query_nullability.py`) names them
//!   and [`Pairs`] refuses an empty one before binding;
//! * `bool` is `bool.TryParse`: either case, surrounding whitespace ignored
//!   (`True` — the spelling `StreamInfo::to_url` emits — was a 400 here);
//! * integers and floats are trimmed before parsing (`NumberStyles.Integer`
//!   / `Float` allow surrounding whitespace);
//! * an enum name matches ignoring case, and its underlying number binds too
//!   (`EnumConverter` → `Enum.Parse(…, true)`) — by its C# value, from Jellyfin's
//!   reflection inventory, never serde's variant index. A number no member has
//!   is a 400, as `EnumTypeModelBinder` refuses an undefined value;
//! * an integer also takes hex as `0x…`, `#…` or `&h…` (`BaseNumberConverter`,
//!   then `Convert.ToInt32(text, 16)`, which takes a `+` and a second `0x` and
//!   reads `0xFFFFFFFF` as -1); floats take none (all measured on 12.2).
//!
//! A NULLABLE enum member goes through Jellyfin's `NullableEnumModelBinder`,
//! which binds an unconvertible value to null instead of failing; that needs
//! the member's type, so those members opt in with
//! `handlers::query_parse::nullable_enum`. Errors keep `serde_urlencoded`'s
//! type, so a rejection is the same 400 as before.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use serde::de::value::{MapDeserializer, StrDeserializer, U32Deserializer};
use serde::de::{self, IntoDeserializer, Visitor};
use serde::forward_to_deserialize_any;
use serde_urlencoded::de::Error;

/// A decoded query string bound as a struct: keys verbatim (already folded to
/// the member names), values through [`Value`]. `required` names the members
/// whose C# type is non-nullable (an empty value is then a 400).
pub(super) struct Pairs<'a> {
    /// The decoded pairs.
    pub(super) pairs: form_urlencoded::Parse<'a>,
    /// The operation's non-nullable members.
    pub(super) required: &'a [String],
}

impl<'de> de::Deserializer<'de> for Pairs<'_> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        // Keys and values stay as `form_urlencoded` decoded them: borrowed
        // unless decoding changed something.
        // `CheckModel` runs for every non-nullable parameter of the action,
        // modelled by the DTO or not, on its (first) value.
        // Only a key's FIRST occurrence binds (`ValueProviderResult.FirstValue`),
        // so only that one is checked; a member the DTO does not model reaches
        // here unreduced, every occurrence still present.
        if !self.required.is_empty() {
            let mut seen: Vec<Cow<'_, str>> = Vec::new();
            for (key, text) in self.pairs {
                if seen.iter().any(|k| k.eq_ignore_ascii_case(&key)) {
                    continue;
                }
                if text.trim().is_empty()
                    && self.required.iter().any(|r| r.eq_ignore_ascii_case(&key))
                {
                    return Err(de::Error::custom(format!(
                        "{key}: The value '{text}' is invalid."
                    )));
                }
                seen.push(key);
            }
        }
        let mut map = MapDeserializer::new(self.pairs.map(|(key, text)| (key, Value { text })));
        let value = visitor.visit_map(&mut map)?;
        map.end()?;
        Ok(value)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// One decoded query value.
pub(crate) struct Value<'a> {
    /// The decoded text.
    text: Cow<'a, str>,
}

impl<'a> Value<'a> {
    /// A value outside a query map (`handlers::query_parse` helpers).
    pub(crate) fn nullable(text: impl Into<Cow<'a, str>>) -> Self {
        Self { text: text.into() }
    }
}

impl IntoDeserializer<'_, Error> for Value<'_> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

/// `BaseNumberConverter`'s hex forms: the digits after `#`, `0x` or `&h`
/// (either case), if `text` has one of them.
fn hex_digits(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix('#') {
        return Some(rest);
    }
    let prefix = text.get(..2)?;
    (prefix.eq_ignore_ascii_case("0x") || prefix.eq_ignore_ascii_case("&h")).then(|| &text[2..])
}

/// `ParseNumbers.StringToInt` at radix 16: an optional `+` (a `-` is refused),
/// then an optional `0x`, then the digits.
fn radix16_digits(text: &str) -> Option<&str> {
    let text = text.strip_prefix('+').unwrap_or(text);
    let text = match text.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("0x") => &text[2..],
        _ => text,
    };
    // `from_str_radix` would take a sign of its own.
    (!text.starts_with(['+', '-'])).then_some(text)
}

/// `deserialize_<integer>` for [`Value`]: trimmed, then parsed as decimal or
/// (with a hex prefix) as the same-width unsigned value reinterpreted, as
/// `Convert.ToInt32(text, 16)` reads `FFFFFFFF` as -1.
macro_rules! parse_integer {
    ($($method:ident => $visit:ident: $ty:ty as $unsigned:ty),* $(,)?) => {$(
        #[allow(clippy::cast_possible_wrap)]
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            let text = self.text.trim();
            let parsed = match hex_digits(text) {
                Some(rest) => match radix16_digits(rest) {
                    Some(digits) => <$unsigned>::from_str_radix(digits, 16).map(|n| n as $ty),
                    None => return Err(de::Error::custom(format!("invalid hex number `{text}`"))),
                },
                None => text.parse::<$ty>(),
            };
            match parsed {
                Ok(n) => visitor.$visit(n),
                Err(e) => Err(de::Error::custom(e)),
            }
        }
    )*};
}

/// `deserialize_<float>` for [`Value`]: trimmed, then parsed (no hex).
macro_rules! parse_float {
    ($($method:ident => $visit:ident: $ty:ty),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            match self.text.trim().parse::<$ty>() {
                Ok(n) => visitor.$visit(n),
                Err(e) => Err(de::Error::custom(e)),
            }
        }
    )*};
}

/// Jellyfin's enums by name: each member's C# value (the 12.2 reflection
/// inventory, the same fixture the model's wire enums are checked against).
fn csharp_enums() -> &'static HashMap<String, Vec<(String, i64)>> {
    static ENUMS: OnceLock<HashMap<String, Vec<(String, i64)>>> = OnceLock::new();
    ENUMS.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct Row {
            name: String,
            members: serde_json::Map<String, serde_json::Value>,
        }
        // A malformed row would silently fall back to declaration order, which
        // is wrong for a gapped enum, so the fixture must parse whole (the
        // `query_enums_bind_every_inventory_value` test runs this).
        include_str!("../../tests/data/json-binding/jellyfin-12.2-enums.jsonl")
            .lines()
            .map(|line| {
                let row: Row = serde_json::from_str(line).expect("enum inventory row");
                let members = row
                    .members
                    .into_iter()
                    .map(|(name, value)| {
                        let value = value.as_i64().expect("enum inventory value");
                        (name, value)
                    })
                    .collect();
                (row.name, members)
            })
            .collect()
    })
}

impl<'de> de::Deserializer<'de> for Value<'_> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_string(self.text.into_owned())
    }

    /// `ConvertEmptyStringToNull`: nothing but whitespace is `null` (a
    /// non-nullable member was already refused by [`Pairs`]).
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.text.trim().is_empty() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    /// Unknown members are skipped without copying their value.
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    /// `bool.TryParse`: `true`/`false` in any case, surrounding whitespace ignored.
    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let text = self.text.trim();
        if text.eq_ignore_ascii_case("true") {
            visitor.visit_bool(true)
        } else if text.eq_ignore_ascii_case("false") {
            visitor.visit_bool(false)
        } else {
            // `serde_urlencoded`'s own wording, so the 400 body is unchanged.
            Err(de::Error::custom(
                "provided string was not `true` or `false`",
            ))
        }
    }

    parse_integer! {
        deserialize_i8 => visit_i8: i8 as u8,
        deserialize_i16 => visit_i16: i16 as u16,
        deserialize_i32 => visit_i32: i32 as u32,
        deserialize_i64 => visit_i64: i64 as u64,
        deserialize_i128 => visit_i128: i128 as u128,
        deserialize_u8 => visit_u8: u8 as u8,
        deserialize_u16 => visit_u16: u16 as u16,
        deserialize_u32 => visit_u32: u32 as u32,
        deserialize_u64 => visit_u64: u64 as u64,
        deserialize_u128 => visit_u128: u128 as u128,
    }

    parse_float! {
        deserialize_f32 => visit_f32: f32,
        deserialize_f64 => visit_f64: f64,
    }

    /// `Enum.Parse(…, ignoreCase: true)` over the variant names (aliases
    /// included); an unknown name keeps the client's spelling for the error.
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let text = self.text.trim();
        // `Enum.Parse` also takes the underlying number (wire enums parse
        // numbers themselves). A Jellyfin enum binds the member with that C#
        // value; an enum outside the inventory has its values in declaration
        // order, so the index is the value.
        if let Ok(number) = text.parse::<i64>() {
            let Some(members) = csharp_enums().get(name) else {
                return match u32::try_from(number) {
                    Ok(index) => visitor.visit_enum(U32Deserializer::<Error>::new(index)),
                    Err(_) => Err(de::Error::custom(format!("The value '{text}' is invalid."))),
                };
            };
            let member = members
                .iter()
                .find(|(_, value)| *value == number)
                .and_then(|(member, _)| variants.iter().find(|v| v.eq_ignore_ascii_case(member)));
            return match member {
                Some(variant) => visitor.visit_enum(StrDeserializer::<Error>::new(variant)),
                None => Err(de::Error::custom(format!("The value '{text}' is invalid."))),
            };
        }
        let name = variants
            .iter()
            .find(|v| v.eq_ignore_ascii_case(text))
            .copied()
            .unwrap_or(text);
        visitor.visit_enum(StrDeserializer::<Error>::new(name))
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct
        map struct identifier
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::Pairs;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "camelCase")]
    enum Sort {
        Name,
        DateCreated,
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(rename_all = "camelCase", default)]
    struct Q {
        is_favorite: Option<bool>,
        limit: Option<i32>,
        search_term: Option<String>,
        sort: Option<Sort>,
        plain: String,
    }

    fn bind_with(query: &str, required: &[String]) -> Result<Q, String> {
        serde_path_to_error::deserialize(Pairs {
            pairs: form_urlencoded::parse(query.as_bytes()),
            required,
        })
        .map_err(|e| e.to_string())
    }

    fn bind(query: &str) -> Result<Q, String> {
        bind_with(query, &[])
    }

    #[test]
    fn strings_are_not_trimmed() {
        let q = bind("searchTerm=%20a%20").expect("binds");
        assert_eq!(q.search_term.as_deref(), Some(" a "));
    }

    #[test]
    fn an_empty_non_nullable_member_is_rejected() {
        // `bool enableTotalRecordCount = true` upstream: `?enableTotalRecordCount=`
        // is a 400 ("The value '' is invalid."), measured on Jellyfin 12.2.
        let required = ["isFavorite".to_owned(), "LIMIT".to_owned()];
        for query in ["isFavorite=", "limit=%20"] {
            let error = bind_with(query, &required).expect_err(query);
            assert!(error.contains("The value"), "{query}: {error}");
        }
        // Absent is still the default; present is parsed as usual.
        let q = bind_with("limit=4", &required).expect("binds");
        assert_eq!((q.is_favorite, q.limit), (None, Some(4)));
        // A non-nullable member the DTO does not model is refused too.
        let required = ["enableRedirection".to_owned()];
        let error = bind_with("enableredirection=&limit=4", &required).expect_err("unmodelled");
        assert!(error.contains("The value"), "{error}");
        assert!(bind_with("enableRedirection=true&enableRedirection=", &required).is_ok());
        assert!(bind_with("ENABLEREDIRECTION=&enableRedirection=true", &required).is_err());
    }

    #[test]
    fn values_bind_like_simple_type_model_binder() {
        let q = bind("isFavorite=True&limit=%2010%20&searchTerm=x&sort=DATECREATED&plain=")
            .expect("binds");
        assert_eq!(q.is_favorite, Some(true));
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.search_term.as_deref(), Some("x"));
        assert_eq!(q.sort, Some(Sort::DateCreated));
        assert_eq!(
            bind("sort=1").expect("numeric").sort,
            Some(Sort::DateCreated)
        );
        assert_eq!(q.plain, "");
        for empty in [
            "isFavorite=&limit=&searchTerm=&sort=",
            "isFavorite=%20&limit=+",
        ] {
            let q = bind(empty).expect(empty);
            assert_eq!(
                (q.is_favorite, q.limit, q.search_term, q.sort),
                (None, None, None, None)
            );
        }
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default)]
    struct Numbers {
        int: Option<i32>,
        long: Option<i64>,
        byte: Option<u8>,
        float: Option<f64>,
    }

    fn numbers(query: &str) -> Result<Numbers, String> {
        serde_path_to_error::deserialize(Pairs {
            pairs: form_urlencoded::parse(query.as_bytes()),
            required: &[],
        })
        .map_err(|e| e.to_string())
    }

    #[test]
    fn integers_take_base_number_converter_hex() {
        // Measured on Jellyfin 12.2 (`?limit=0x2`, `%232`, `%26h2` all page 2).
        for (query, int) in [
            ("int=0x10", 16),
            ("int=0X10", 16),
            ("int=%2310", 16),
            ("int=%26h10", 16),
            ("int=%26H1f", 31),
            ("int=%20%200x2%20", 2),
            ("int=0xFFFFFFFF", -1),
            ("int=0x7FFFFFFF", i32::MAX),
            // `ParseNumbers.StringToInt` skips a `+` and a second `0x`.
            ("int=0x%2B2", 2),
            ("int=0x0x2", 2),
            ("int=%230x2", 2),
            ("int=%23%2B2", 2),
            ("int=%26h0X2", 2),
        ] {
            assert_eq!(numbers(query).expect(query).int, Some(int), "{query}");
        }
        assert_eq!(numbers("long=0xFFFFFFFF").unwrap().long, Some(0xFFFF_FFFF));
        assert_eq!(numbers("byte=0xff").unwrap().byte, Some(255));
        for query in [
            "int=0x",
            "int=0xg",
            "int=0x-1",
            "int=0x%2B-1",
            "int=%2B0x2",
            "int=0x%202",
            "int=0x100000000",
            "float=0x1",
            "float=%26h2",
            "int=1.0",
        ] {
            assert!(numbers(query).is_err(), "{query}");
        }
    }

    /// Named after a Jellyfin enum whose C# values skip 0 and 6.
    #[allow(clippy::enum_variant_names)]
    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename = "ItemFilter")]
    enum Filter {
        IsFolder,
        IsNotFolder,
        IsResumable,
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default)]
    struct F {
        filter: Option<Filter>,
    }

    #[test]
    fn enum_numbers_bind_by_csharp_value() {
        let bind = |query: &str| -> Result<F, String> {
            serde_path_to_error::deserialize(Pairs {
                pairs: form_urlencoded::parse(query.as_bytes()),
                required: &[],
            })
            .map_err(|e| e.to_string())
        };
        assert_eq!(bind("filter=1").unwrap().filter, Some(Filter::IsFolder));
        assert_eq!(
            bind("filter=%2B2").unwrap().filter,
            Some(Filter::IsNotFolder)
        );
        assert_eq!(bind("filter=7").unwrap().filter, Some(Filter::IsResumable));
        // `EnumTypeModelBinder`: a value no member has, or a negative one.
        for query in ["filter=0", "filter=6", "filter=-1", "filter=0x1"] {
            assert!(bind(query).is_err(), "{query}");
        }
        // An enum outside the inventory keeps declaration order.
        assert_eq!(
            bind_with("sort=1", &[]).unwrap().sort,
            Some(Sort::DateCreated)
        );
    }

    /// Every serde-derived enum a contract query member reaches binds each C#
    /// value to the member of that name (the wire enums are checked by the
    /// model's reflection test).
    #[test]
    fn query_enums_bind_every_inventory_value() {
        fn check<T: serde::de::DeserializeOwned + std::fmt::Debug + PartialEq>(name: &str) {
            let members = &super::csharp_enums()[name];
            assert!(!members.is_empty(), "{name}");
            for (member, value) in members {
                let by_value = T::deserialize(super::Value::nullable(value.to_string()));
                let by_name = T::deserialize(super::Value::nullable(member.clone()))
                    .unwrap_or_else(|e| panic!("{name}.{member}: {e}"));
                assert_eq!(by_value.ok(), Some(by_name), "{name}.{member} = {value}");
            }
        }
        check::<ferrofin_model::querying::ItemFilter>("ItemFilter");
        check::<ferrofin_model::drawing::ImageFormat>("ImageFormat");
        check::<ferrofin_model::entities::SeriesStatus>("SeriesStatus");
        check::<ferrofin_model::entities::CollectionTypeOptions>("CollectionTypeOptions");
        check::<ferrofin_model::session::PlayCommand>("PlayCommand");
        check::<ferrofin_traits::activity::ActivityLogSortBy>("ActivityLogSortBy");
        let lines = include_str!("../../tests/data/json-binding/jellyfin-12.2-enums.jsonl")
            .lines()
            .count();
        assert_eq!(
            super::csharp_enums().len(),
            lines,
            "every inventory row parses"
        );
    }

    #[test]
    fn unparsable_values_are_still_rejected_by_path() {
        for (query, path) in [
            ("isFavorite=yes", "isFavorite"),
            ("limit=ten", "limit"),
            ("sort=Nope", "sort"),
        ] {
            let error = bind(query).expect_err(query);
            assert!(error.starts_with(path), "{query}: {error}");
        }
    }
}
