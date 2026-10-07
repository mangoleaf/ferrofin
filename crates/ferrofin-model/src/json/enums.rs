//! Jellyfin's string enum converter, including C#'s unnamed integer values.
//!
//! Discriminants come from the versioned upstream inventory, never serde's
//! variant indexes. Even non-flags enums accept comma-separated names (bitwise
//! OR), as `Enum.TryParse` does. Only enum-level `DefaultValue` attributes give
//! `null` and the empty string a default; property-level attributes do not.

use std::marker::PhantomData;

use serde::Deserializer;
use serde::de::{self, Visitor};

/// A C# enum whose JSON representation retains unnamed integer values.
pub trait JsonEnum: Sized {
    /// Constructs a declared variant or retains the unnamed discriminant.
    fn from_discriminant(value: i32) -> Self;
    /// Declared JSON names paired with their C# discriminants.
    fn members() -> &'static [(&'static str, i32)];
    /// The enum-level `DefaultValue`, if one is declared upstream.
    fn json_default() -> Option<i32>;
}

/// Deserializes an enum using Jellyfin's `JsonStringEnumConverter` rules.
///
/// # Errors
/// Rejects invalid token kinds, unknown names and values outside `Int32`.
pub fn deserialize<'de, D: Deserializer<'de>, T: JsonEnum>(d: D) -> Result<T, D::Error> {
    d.deserialize_any(EnumVisitor::<T>(PhantomData))
}

struct EnumVisitor<T>(PhantomData<T>);

impl<T: JsonEnum> Visitor<'_> for EnumVisitor<T> {
    type Value = T;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an enum name or a 32-bit integer")
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<T, E> {
        i32::try_from(value)
            .map(T::from_discriminant)
            .map_err(E::custom)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<T, E> {
        i32::try_from(value)
            .map(T::from_discriminant)
            .map_err(E::custom)
    }

    fn visit_unit<E: de::Error>(self) -> Result<T, E> {
        T::json_default()
            .map(T::from_discriminant)
            .ok_or_else(|| E::invalid_type(de::Unexpected::Unit, &self))
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<T, E> {
        if text.is_empty()
            && let Some(default) = T::json_default()
        {
            return Ok(T::from_discriminant(default));
        }
        let trimmed = text.trim();
        if let Ok(number) = trimmed.parse::<i32>() {
            return Ok(T::from_discriminant(number));
        }
        let mut combined = 0;
        for name in trimmed.split(',') {
            let Some((_, value)) = T::members()
                .iter()
                .find(|(member, _)| member.eq_ignore_ascii_case(name.trim()))
            else {
                return Err(E::invalid_value(de::Unexpected::Str(text), &self));
            };
            combined |= value;
        }
        Ok(T::from_discriminant(combined))
    }
}

/// Keeps the binding table beside the enum declaration. Every generated table
/// is checked against the upstream reflection fixture by model tests.
macro_rules! wire_enum {
    ($name:ident, $default:expr, {$($variant:ident => ($wire:literal, $value:literal)),* $(,)?}) => {
        impl $name {
            /// The C# enum name, or its decimal integer when no member names it.
            #[must_use]
            pub fn json_name(self) -> std::borrow::Cow<'static, str> {
                match self {
                    $(Self::$variant => std::borrow::Cow::Borrowed($wire),)*
                    Self::Unrecognized(value) => std::borrow::Cow::Owned(value.to_string()),
                }
            }

            /// The C# underlying integer, including values without a declared name.
            #[must_use]
            pub const fn json_value(self) -> i32 {
                match self {
                    $(Self::$variant => $value,)*
                    Self::Unrecognized(value) => value,
                }
            }

            /// Constructs a declared variant or preserves an unnamed C# value.
            #[must_use]
            pub const fn from_json_value(value: i32) -> Self {
                match value {
                    $($value => Self::$variant,)*
                    other => Self::Unrecognized(other),
                }
            }
        }

        impl $crate::json::enums::JsonEnum for $name {
            fn from_discriminant(value: i32) -> Self { Self::from_json_value(value) }
            fn members() -> &'static [(&'static str, i32)] { &[$(($wire, $value)),*] }
            fn json_default() -> Option<i32> { $default }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                $crate::json::enums::deserialize(d)
            }
        }
    };
}
pub(crate) use wire_enum;

#[cfg(test)]
mod tests {
    #[derive(serde::Serialize)]
    enum Sparse {
        First,
        Second,
        #[serde(untagged)]
        Unrecognized(i32),
    }
    wire_enum!(Sparse, None, { First => ("First", 1), Second => ("Second", 4) });

    #[derive(serde::Serialize)]
    enum WithDefault {
        First,
        Second,
        #[serde(untagged)]
        Unrecognized(i32),
    }
    wire_enum!(WithDefault, Some(1), { First => ("First", 1), Second => ("Second", 4) });

    #[test]
    fn enum_binding_matches_jellyfin_oracle() {
        let fixture =
            include_str!("../../../ferrofin-api/tests/data/json-binding/jellyfin-12.2.jsonl");
        let mut checked = 0;
        for line in fixture.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = row["input"].as_str().unwrap();
            // serde_json's ordinary reader classifies bare -0 as a float.
            // The request Doc retains the lexeme and supplies integer zero;
            // the extractor tests pin that lexical distinction from -0.0.
            let model_input = if input == "-0" { "0" } else { input };
            let actual = match row["type"].as_str().unwrap() {
                "enum" => {
                    serde_json::from_str::<Sparse>(model_input).and_then(serde_json::to_value)
                }
                "default_enum" => {
                    serde_json::from_str::<WithDefault>(model_input).and_then(serde_json::to_value)
                }
                _ => continue,
            };
            assert_eq!(
                actual.is_ok(),
                row["accepted"].as_bool().unwrap(),
                "{input}: {actual:?}"
            );
            if let Ok(value) = actual {
                let text = row["value"].as_str().unwrap();
                let expected = text.parse::<i32>().map_or_else(
                    |_| serde_json::Value::String(text.into()),
                    serde_json::Value::from,
                );
                assert_eq!(value, expected, "{input}");
            }
            checked += 1;
        }
        assert_eq!(checked, 218);
        assert_eq!(Sparse::from_json_value(5).json_value(), 5);
        assert_eq!(Sparse::from_json_value(5).json_name(), "5");
        assert_eq!(WithDefault::from_json_value(1).json_value(), 1);
        assert_eq!(WithDefault::from_json_value(1).json_name(), "First");
    }
}
