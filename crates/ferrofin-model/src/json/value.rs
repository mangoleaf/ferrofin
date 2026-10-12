//! Type markers for value conversions that cannot be inferred from serde's
//! `deserialize_option` or `deserialize_string` callbacks alone.

use serde::de::Visitor;
use serde::{Deserialize, Deserializer};
use std::marker::PhantomData;

/// Newtype name identifying nullable C# value types to the request binder.
pub const NULLABLE_VALUE: &str = "$ferrofin::nullable_value";
/// Newtype name requiring a string token (GUID/date converters bypass coercion).
pub const STRING_TOKEN: &str = "$ferrofin::string_token";

/// Reads a nullable C# value type. The request binder turns an exact empty
/// string into null before invoking the inner type's deserializer.
///
/// # Errors
/// Propagates the inner type's conversion error for every other value.
pub fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Nullable<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Nullable<T> {
        type Value = Option<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a nullable value")
        }
        fn visit_newtype_struct<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            Option::<T>::deserialize(d)
        }
    }
    deserializer.deserialize_newtype_struct(NULLABLE_VALUE, Nullable(PhantomData))
}

/// A string read through a converter that requires an actual JSON string.
/// Numeric/boolean coercion is specific to string members, not GUIDs or dates.
pub(crate) struct StringToken(pub(crate) String);

impl<'de> Deserialize<'de> for StringToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Token;
        impl<'de> Visitor<'de> for Token {
            type Value = StringToken;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON string token")
            }
            fn visit_newtype_struct<D: Deserializer<'de>>(
                self,
                d: D,
            ) -> Result<Self::Value, D::Error> {
                String::deserialize(d).map(StringToken)
            }
        }
        deserializer.deserialize_newtype_struct(STRING_TOKEN, Token)
    }
}
