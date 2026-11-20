//! `JsonDelimitedCollectionConverter<T>` — the body converter Jellyfin puts on
//! a collection member through `JsonCommaDelimitedCollectionConverterFactory`
//! (or, on `GetProgramsDto.Genres` alone, the pipe factory). Port of v12.4
//! `src/Jellyfin.Extensions/Json/Converters/JsonDelimitedCollectionConverter.cs`.
//!
//! A JSON **string** is split on the delimiter with
//! `StringSplitOptions.RemoveEmptyEntries`, each piece is `Trim()`med and
//! converted with the element type's `TypeConverter`, and a piece the converter
//! refuses is dropped (`catch (FormatException)`), so `""` is no elements. Any
//! other token is `JsonSerializer.Deserialize<T[]>`: an array whose elements
//! bind as strictly as anywhere else.
//!
//! A piece converts through the element's own `Deserialize` from a string,
//! which is that `TypeConverter` for every element type upstream decorates: a
//! wire enum is `EnumConverter`'s `Enum.Parse(…, ignoreCase: true)`
//! ([`super::enums::try_parse`], an undeclared number kept), a GUID is
//! `GuidConverter` ([`super::guid::comma_delimited`]), a string is itself.
//!
//! `null` never reaches the converter (`HandleNull` is false for a collection):
//! the member binds `null`, which a nullable member ([`comma_option`],
//! [`pipe_option`]) reads as `None` and MVC's implicit `[Required]` refuses
//! for a non-nullable one ([`comma`]), so that is an error.

use std::fmt;
use std::marker::PhantomData;

use serde::Deserializer;
use serde::de::value::StrDeserializer;
use serde::de::{self, DeserializeOwned, SeqAccess, Visitor};

/// A non-nullable member (`IReadOnlyList<T> X { get; set; } = []`) with the
/// comma factory: an array, or a comma-delimited string.
///
/// # Errors
///
/// Fails on `null`, on a token that is neither an array nor a string, and on
/// an array element that does not bind.
pub fn comma<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    deserializer.deserialize_any(Delimited::new(','))
}

/// A nullable member (`IReadOnlyList<T>? X`) with the comma factory: `null` is
/// `None`, otherwise as [`comma`].
///
/// # Errors
///
/// Fails on a token that is neither an array, a string nor `null`, and on an
/// array element that does not bind.
pub fn comma_option<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    deserializer.deserialize_option(Nullable(Delimited::new(',')))
}

/// A nullable member with the **pipe** factory: `"News,Sport"` is one element.
///
/// # Errors
///
/// As [`comma_option`].
pub fn pipe_option<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    deserializer.deserialize_option(Nullable(Delimited::new('|')))
}

/// The `null` layer of a nullable member.
struct Nullable<T>(Delimited<T>);

impl<'de, T: DeserializeOwned> Visitor<'de> for Nullable<T> {
    type Value = Option<Vec<T>>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.expecting(formatter)?;
        formatter.write_str(", or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self.0).map(Some)
    }
}

/// The converter's `Read`: the string arm splits leniently, the array arm is
/// strict.
struct Delimited<T> {
    /// The character the string form is split on.
    delimiter: char,
    /// Ties the visitor to the element type without owning one.
    marker: PhantomData<fn() -> T>,
}

impl<T> Delimited<T> {
    const fn new(delimiter: char) -> Self {
        Self {
            delimiter,
            marker: PhantomData,
        }
    }
}

impl<'de, T: DeserializeOwned> Visitor<'de> for Delimited<T> {
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "an array or a {:?}-delimited string",
            self.delimiter
        )
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        // `Split(Delimiter, RemoveEmptyEntries)`, then `Trim()`: a whitespace-only
        // piece reaches the converter as "", which a string keeps and a GUID or
        // an enum drops.
        // shortcut: a wire enum with an enum-level `DefaultValue` would read "" as
        // that default where `EnumConverter` drops it; no decorated element type
        // has one, so special-case "" if one ever does.
        Ok(value
            .split(self.delimiter)
            .filter(|piece| !piece.is_empty())
            .filter_map(|piece| {
                T::deserialize(StrDeserializer::<de::value::Error>::new(piece.trim())).ok()
            })
            .collect())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::with_capacity(seq.size_hint().unwrap_or_default());
        while let Some(value) = seq.next_element()? {
            values.push(value);
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde::Deserialize;
    use uuid::Uuid;

    use crate::session::GeneralCommandType::{self, MoveDown, MoveUp};

    /// Upstream's `GenericBodyIReadOnlyListModel<T>` / `GenericBodyArrayModel<T>`.
    #[derive(Debug, Deserialize)]
    struct Model<T: serde::de::DeserializeOwned> {
        #[serde(rename = "Value", deserialize_with = "super::comma_option")]
        value: Option<Vec<T>>,
    }

    fn bind<T: serde::de::DeserializeOwned>(json: &str) -> Option<Vec<T>> {
        serde_json::from_str::<Model<T>>(json).expect(json).value
    }

    /// `JsonCommaDelimitedCollectionTests` and
    /// `JsonCommaDelimitedIReadOnlyListTests` (v12.4) over `string`. The two
    /// `List<T>` cases (`Deserialize_EmptyList_Success`,
    /// `Deserialize_StringList_Valid_Success`) assert that the factory refuses a
    /// `List<T>` member, which has no Rust counterpart.
    #[rstest]
    #[case::string_null(r#"{ "Value": null }"#, None)]
    #[case::empty(r#"{ "Value": "" }"#, Some(&[][..]))]
    #[case::string_valid(r#"{ "Value": "a,b,c" }"#, Some(&["a", "b", "c"][..]))]
    #[case::string_space_valid(r#"{ "Value": "a, b, c" }"#, Some(&["a", "b", "c"][..]))]
    #[case::string_array_valid(r#"{ "Value": ["a","b","c"] }"#, Some(&["a", "b", "c"][..]))]
    // `RemoveEmptyEntries` before `Trim()`: a whitespace-only piece is a "" the
    // string converter keeps.
    #[case::whitespace_piece(r#"{ "Value": "a, ,b,," }"#, Some(&["a", "", "b"][..]))]
    fn strings_bind_like_upstream(#[case] json: &str, #[case] expected: Option<&[&str]>) {
        let expected = expected.map(|values| values.iter().map(|&v| v.to_owned()).collect());
        assert_eq!(bind::<String>(json), expected);
    }

    /// The same suites over `GeneralCommandType`.
    #[rstest]
    #[case::valid(r#"{ "Value": "MoveUp,MoveDown" }"#)]
    #[case::empty_entry(r#"{ "Value": "MoveUp,,MoveDown" }"#)]
    #[case::invalid(r#"{ "Value": "MoveUp,TotallyNotAValidCommand,MoveDown" }"#)]
    #[case::space_valid(r#"{ "Value": "MoveUp, MoveDown" }"#)]
    // A whitespace-only piece is `Enum.Parse("")`, a FormatException.
    #[case::whitespace_entry(r#"{ "Value": "MoveUp, ,MoveDown" }"#)]
    #[case::array_valid(r#"{ "Value": ["MoveUp", "MoveDown"] }"#)]
    // `EnumConverter` is `Enum.Parse(…, ignoreCase: true)`, which takes the C#
    // value too (`MoveDown` is 1).
    #[case::any_case(r#"{ "Value": "moveup, MOVEDOWN" }"#)]
    #[case::number(r#"{ "Value": "0,1" }"#)]
    fn general_commands_bind_like_upstream(#[case] json: &str) {
        assert_eq!(
            bind::<GeneralCommandType>(json),
            Some(vec![MoveUp, MoveDown])
        );
    }

    #[test]
    fn an_undeclared_number_is_kept() {
        // `Enum.Parse("99")` returns `(GeneralCommandType)99`; no FormatException.
        assert_eq!(
            bind::<GeneralCommandType>(r#"{ "Value": "99" }"#),
            Some(vec![GeneralCommandType::from_json_value(99)])
        );
    }

    #[rstest]
    #[case::unknown_name(r#"{ "Value": ["Bogus"] }"#)]
    #[case::number_token(r#"{ "Value": 5 }"#)]
    #[case::object_token(r#"{ "Value": {} }"#)]
    fn other_tokens_are_strict(#[case] json: &str) {
        assert!(serde_json::from_str::<Model<GeneralCommandType>>(json).is_err());
    }

    /// A non-nullable member: `null` is the implicit `[Required]` refusal, an
    /// absent one keeps its `= []` initializer.
    #[test]
    fn a_non_nullable_member_refuses_null() {
        #[derive(Debug, Deserialize)]
        struct Required {
            #[serde(default, rename = "Value", deserialize_with = "super::comma")]
            value: Vec<GeneralCommandType>,
        }
        let read = |json| serde_json::from_str::<Required>(json).map(|r| r.value);
        assert_eq!(read(r#"{"Value":"MoveUp"}"#).unwrap(), vec![MoveUp]);
        assert_eq!(read(r#"{"Value":[]}"#).unwrap(), vec![]);
        assert_eq!(read("{}").unwrap(), vec![]);
        assert!(read(r#"{"Value":null}"#).is_err());
    }

    #[test]
    fn the_pipe_factory_splits_on_the_pipe_only() {
        #[derive(Debug, Deserialize)]
        struct Genres {
            #[serde(rename = "Value", deserialize_with = "super::pipe_option")]
            value: Option<Vec<String>>,
        }
        let read = |json| serde_json::from_str::<Genres>(json).unwrap().value;
        assert_eq!(
            read(r#"{"Value":"News|Sport"}"#),
            Some(vec!["News".to_owned(), "Sport".to_owned()])
        );
        assert_eq!(
            read(r#"{"Value":"News,Sport"}"#),
            Some(vec!["News,Sport".to_owned()])
        );
    }

    /// `GuidConverter` for a piece (trimmed, any `Guid.Parse` spelling, a
    /// malformed one dropped), `JsonGuidConverter` for an array element (`null`
    /// is `Guid.Empty`, a malformed one an error).
    #[test]
    fn guid_members_convert_pieces_and_elements_like_upstream() {
        #[derive(Debug, Deserialize)]
        struct Ids {
            #[serde(default, with = "crate::json::guid::comma_delimited")]
            ids: Vec<Uuid>,
            #[serde(default, with = "crate::json::guid::comma_delimited::option")]
            maybe: Option<Vec<Uuid>>,
        }
        let id = Uuid::from_u128(0xd37e_cb9d_75b0_c0a8_e9ec_b0a8_64ec_670e);
        let read =
            |json: String| serde_json::from_str::<Ids>(&json).map(|ids| (ids.ids, ids.maybe));
        assert_eq!(
            read(format!(
                r#"{{"ids":" {id} ,not-a-guid, ,({id}),{}"}}"#,
                id.simple()
            ))
            .unwrap(),
            (vec![id, id, id], None)
        );
        assert_eq!(
            read(format!(r#"{{"ids":["{id}",null],"maybe":"zzz"}}"#)).unwrap(),
            (vec![id, Uuid::nil()], Some(vec![]))
        );
        assert_eq!(
            read(r#"{"maybe":null}"#.to_owned()).unwrap(),
            (vec![], None)
        );
        assert!(read(r#"{"ids":["zzz"]}"#.to_owned()).is_err());
        assert!(read(r#"{"ids":null}"#.to_owned()).is_err());
    }

    /// The converter's `Write` is a plain array: `Serialize_*` in both suites,
    /// for the derived write of an enum member and for the GUID one.
    #[test]
    fn members_still_write_an_array() {
        #[derive(serde::Serialize)]
        struct Commands {
            #[serde(rename = "Value")]
            value: Vec<GeneralCommandType>,
        }
        #[derive(serde::Serialize)]
        struct Ids {
            #[serde(with = "crate::json::guid::comma_delimited")]
            ids: Vec<Uuid>,
            #[serde(with = "crate::json::guid::comma_delimited::option")]
            maybe: Option<Vec<Uuid>>,
        }
        let commands = Commands {
            value: vec![MoveUp, MoveDown],
        };
        assert_eq!(
            serde_json::to_string(&commands).unwrap(),
            r#"{"Value":["MoveUp","MoveDown"]}"#
        );
        let ids = Ids {
            ids: vec![Uuid::from_u128(1)],
            maybe: None,
        };
        assert_eq!(
            serde_json::to_string(&ids).unwrap(),
            r#"{"ids":["00000000000000000000000000000001"],"maybe":null}"#
        );
    }
}
