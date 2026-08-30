//! The wire spelling of guids and dates — ports of the converters Jellyfin
//! registers globally in `JsonDefaults` (`Jellyfin.Extensions.Json`).
//!
//! Every DTO guid and date goes through these (`#[serde(with = …)]`) so the JSON a
//! client sees is byte-for-byte what Jellyfin writes. That matters beyond looks:
//! clients and the Jellyfin-DB adoption path cache ids as strings and compare
//! them verbatim, so a hyphenated id from Ferrofin would never equal the `N`-form
//! id the same item had under Jellyfin.
//!
//! - [`guid`] — `JsonGuidConverter`: written as `ToString("N")` (32 lowercase
//!   hex digits), read from any spelling; JSON `null` reads as `Guid.Empty`.
//! - [`guid::option`] — `JsonNullableGuidConverter`: `Guid.Empty` is written as
//!   `null`.
//! - [`guid::vec`] / [`guid::option_vec`] — element-wise `JsonGuidConverter`.
//! - [`datetime`] / [`datetime::option`] — `JsonDateTimeConverter`: ISO-8601 with
//!   the 100 ns tick fraction; when the millisecond component is zero all seven
//!   fraction digits are written, otherwise trailing zeros are trimmed (that is
//!   `Utf8JsonWriter.WriteStringValue(DateTime)`).

/// `JsonGuidConverter` — `Uuid` fields.
pub mod guid {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    /// Writes `ToString("N")`.
    ///
    /// # Errors
    ///
    /// Propagates the serializer's error.
    pub fn serialize<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.simple().to_string())
    }

    /// Reads any guid spelling; `null` is `Guid.Empty`.
    ///
    /// # Errors
    ///
    /// Fails when the value is neither a guid string nor `null`.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        Ok(Option::<Uuid>::deserialize(deserializer)?.unwrap_or_default())
    }

    /// `JsonNullableGuidConverter` — `Option<Uuid>` fields.
    pub mod option {
        use serde::{Deserialize, Deserializer, Serializer};
        use uuid::Uuid;

        /// `None` and `Guid.Empty` are `null`; anything else `ToString("N")`.
        ///
        /// # Errors
        ///
        /// Propagates the serializer's error.
        pub fn serialize<S: Serializer>(
            value: &Option<Uuid>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(id) if !id.is_nil() => serializer.serialize_str(&id.simple().to_string()),
                _ => serializer.serialize_none(),
            }
        }

        /// Reads any guid spelling; `null`/absent is `None`. An empty string is
        /// an error: `JsonNullableGuidConverter` is registered ahead of the
        /// nullable-struct factory, so a `Guid?` never gets the `""`→null leniency.
        ///
        /// # Errors
        ///
        /// Fails when the value is neither a guid string nor `null`.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Uuid>, D::Error> {
            Option::<Uuid>::deserialize(deserializer)
        }
    }

    /// Element-wise `JsonGuidConverter` — `Vec<Uuid>` fields.
    pub mod vec {
        use serde::ser::SerializeSeq;
        use serde::{Deserialize, Deserializer, Serializer};
        use uuid::Uuid;

        /// Writes each element as `ToString("N")`.
        ///
        /// # Errors
        ///
        /// Propagates the serializer's error.
        pub fn serialize<S: Serializer>(value: &[Uuid], serializer: S) -> Result<S::Ok, S::Error> {
            let mut seq = serializer.serialize_seq(Some(value.len()))?;
            for id in value {
                seq.serialize_element(&id.simple().to_string())?;
            }
            seq.end()
        }

        /// Reads each element from any guid spelling.
        ///
        /// # Errors
        ///
        /// Fails when an element is not a guid string.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<Uuid>, D::Error> {
            Vec::<Uuid>::deserialize(deserializer)
        }
    }

    /// Element-wise `JsonGuidConverter` — `Option<Vec<Uuid>>` fields.
    pub mod option_vec {
        use serde::{Deserialize, Deserializer, Serializer};
        use uuid::Uuid;

        /// `None` is `null`; otherwise each element as `ToString("N")`.
        ///
        /// # Errors
        ///
        /// Propagates the serializer's error.
        pub fn serialize<S: Serializer>(
            value: &Option<Vec<Uuid>>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(ids) => super::vec::serialize(ids, serializer),
                None => serializer.serialize_none(),
            }
        }

        /// Reads each element from any guid spelling; `null`/absent is `None`.
        ///
        /// # Errors
        ///
        /// Fails when an element is not a guid string.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Vec<Uuid>>, D::Error> {
            Option::<Vec<Uuid>>::deserialize(deserializer)
        }
    }
}

/// `JsonDateTimeConverter` — `DateTime<Utc>` fields.
pub mod datetime {
    use chrono::{DateTime, NaiveDate, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    /// C# `DateTime.MinValue` — `0001-01-01T00:00:00Z`, what an unassigned
    /// .NET `DateTime` serializes to (`0001-01-01T00:00:00.0000000Z`).
    ///
    /// This is NOT [`chrono::DateTime::<Utc>::MIN_UTC`], which is year
    /// -262144; that constant would produce a date no Jellyfin client has ever
    /// seen.
    ///
    /// # Panics
    ///
    /// Never: `0001-01-01T00:00:00` is a valid date.
    #[must_use]
    pub fn dotnet_min() -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(1, 1, 1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .expect("0001-01-01T00:00:00 is a valid date")
            .and_utc()
    }

    /// `yyyy-MM-ddTHH:mm:ss.<ticks>Z`: all seven tick digits when the millisecond
    /// component is zero (`value.Millisecond == 0`), otherwise trailing zeros
    /// trimmed (`Utf8JsonWriter`'s `FFFFFFF`).
    #[must_use]
    pub fn format(value: &DateTime<Utc>) -> String {
        let ticks = value.timestamp_subsec_nanos() / 100;
        let mut fraction = format!("{ticks:07}");
        if ticks / 10_000 != 0 {
            // Milliseconds present: STJ trims the trailing zeros of the 7-digit
            // fraction (a millisecond is never all zeros here, so some digit stays).
            while fraction.ends_with('0') {
                fraction.pop();
            }
        }
        format!("{}.{fraction}Z", value.format("%Y-%m-%dT%H:%M:%S"))
    }

    /// Writes [`format`].
    ///
    /// # Errors
    ///
    /// Propagates the serializer's error.
    pub fn serialize<S: Serializer>(
        value: &DateTime<Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format(value))
    }

    /// Parses what `Utf8JsonReader.GetDateTime` accepts: an ISO 8601 timestamp
    /// with an offset or `Z`, one without (read as UTC), or a bare date
    /// (midnight UTC — jellyfin-web's metadata editor sends `"2022-01-01"` for an
    /// edited date).
    ///
    /// # Errors
    ///
    /// Returns the parse error of the closest matching form.
    pub fn parse(text: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
        let text = text.trim();
        match DateTime::parse_from_rfc3339(text) {
            Ok(dt) => Ok(dt.with_timezone(&Utc)),
            Err(rfc) => {
                if let Ok(naive) =
                    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                {
                    return Ok(naive.and_utc());
                }
                match chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
                    Ok(date) => Ok(date.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc()),
                    Err(_) => Err(rfc),
                }
            }
        }
    }

    /// Reads a timestamp with [`parse`].
    ///
    /// # Errors
    ///
    /// Fails when the value is not a string or not a timestamp.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<Utc>, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        parse(&text).map_err(serde::de::Error::custom)
    }

    /// `JsonDateTimeConverter` through `JsonNullableStructConverterFactory` —
    /// `Option<DateTime<Utc>>` fields.
    pub mod option {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize, Deserializer, Serializer};

        /// `None` is `null`; otherwise [`super::format`].
        ///
        /// # Errors
        ///
        /// Propagates the serializer's error.
        pub fn serialize<S: Serializer>(
            value: &Option<DateTime<Utc>>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(dt) => serializer.serialize_str(&super::format(dt)),
                None => serializer.serialize_none(),
            }
        }

        /// Reads a timestamp with [`super::parse`]; `null`/absent — and `""`,
        /// which `JsonNullableStructConverter` reads as null because "some
        /// clients send an empty string" — is `None`.
        ///
        /// # Errors
        ///
        /// Fails when the value is neither a timestamp, `""` nor `null`.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<DateTime<Utc>>, D::Error> {
            match Option::<std::borrow::Cow<str>>::deserialize(deserializer)? {
                None => Ok(None),
                Some(text) if text.trim().is_empty() => Ok(None),
                Some(text) => super::parse(&text)
                    .map(Some)
                    .map_err(serde::de::Error::custom),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Ids {
        #[serde(with = "super::guid")]
        id: Uuid,
        #[serde(default, with = "super::guid::option")]
        parent: Option<Uuid>,
        #[serde(default, with = "super::guid::vec")]
        children: Vec<Uuid>,
        #[serde(default, with = "super::guid::option_vec")]
        extra: Option<Vec<Uuid>>,
    }

    const ID: Uuid = Uuid::from_u128(0xd37e_cb9d_75b0_c0a8_e9ec_b0a8_64ec_670e);

    #[test]
    fn guids_are_written_in_the_n_form() {
        let ids = Ids {
            id: ID,
            parent: Some(ID),
            children: vec![ID, Uuid::from_u128(1)],
            extra: Some(vec![ID]),
        };
        assert_eq!(
            serde_json::to_string(&ids).unwrap(),
            r#"{"id":"d37ecb9d75b0c0a8e9ecb0a864ec670e","parent":"d37ecb9d75b0c0a8e9ecb0a864ec670e","children":["d37ecb9d75b0c0a8e9ecb0a864ec670e","00000000000000000000000000000001"],"extra":["d37ecb9d75b0c0a8e9ecb0a864ec670e"]}"#
        );
    }

    #[test]
    fn empty_nullable_guid_is_null_and_null_non_nullable_is_empty() {
        // JsonNullableGuidConverter writes Guid.Empty as null …
        let ids = Ids {
            id: Uuid::nil(),
            parent: Some(Uuid::nil()),
            children: Vec::new(),
            extra: None,
        };
        assert_eq!(
            serde_json::to_string(&ids).unwrap(),
            r#"{"id":"00000000000000000000000000000000","parent":null,"children":[],"extra":null}"#
        );
        // … and JsonGuidConverter reads null as Guid.Empty.
        let read: Ids = serde_json::from_str(r#"{"id":null}"#).unwrap();
        assert_eq!(read.id, Uuid::nil());
        assert_eq!(read.parent, None);
    }

    #[test]
    fn guids_are_read_from_any_spelling() {
        let read: Ids = serde_json::from_str(
            r#"{"id":"D37ECB9D-75B0-C0A8-E9EC-B0A864EC670E","parent":"d37ecb9d75b0c0a8e9ecb0a864ec670e","children":["d37ecb9d-75b0-c0a8-e9ec-b0a864ec670e"]}"#,
        )
        .unwrap();
        assert_eq!(read.id, ID);
        assert_eq!(read.parent, Some(ID));
        assert_eq!(read.children, vec![ID]);
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Dates {
        #[serde(with = "super::datetime")]
        at: DateTime<Utc>,
        #[serde(default, with = "super::datetime::option")]
        maybe: Option<DateTime<Utc>>,
    }

    #[test]
    fn whole_second_dates_carry_all_seven_tick_digits() {
        let at = Utc.with_ymd_and_hms(2022, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(super::datetime::format(&at), "2022-01-01T00:00:00.0000000Z");
        // Sub-millisecond ticks alone still take the "Millisecond == 0" branch.
        let at = at + chrono::Duration::nanoseconds(50_000);
        assert_eq!(super::datetime::format(&at), "2022-01-01T00:00:00.0000500Z");
    }

    #[test]
    fn dates_with_milliseconds_trim_trailing_zeros() {
        let at = Utc.with_ymd_and_hms(2026, 8, 23, 16, 57, 22).unwrap()
            + chrono::Duration::nanoseconds(479_586_000);
        assert_eq!(super::datetime::format(&at), "2026-08-23T16:57:22.479586Z");
        let at = Utc.with_ymd_and_hms(2026, 8, 23, 16, 57, 22).unwrap()
            + chrono::Duration::milliseconds(200);
        assert_eq!(super::datetime::format(&at), "2026-08-23T16:57:22.2Z");
        // Nanoseconds below the 100 ns tick are dropped, as .NET never has them.
        let at = Utc.with_ymd_and_hms(2026, 8, 23, 5, 29, 37).unwrap()
            + chrono::Duration::nanoseconds(764_472_900);
        assert_eq!(super::datetime::format(&at), "2026-08-23T05:29:37.7644729Z");
    }

    #[test]
    fn dates_round_trip_through_the_field_attributes() {
        let dates = Dates {
            at: Utc.with_ymd_and_hms(2022, 1, 1, 0, 0, 0).unwrap(),
            maybe: None,
        };
        let json = serde_json::to_string(&dates).unwrap();
        assert_eq!(
            json,
            r#"{"at":"2022-01-01T00:00:00.0000000Z","maybe":null}"#
        );
        assert_eq!(serde_json::from_str::<Dates>(&json).unwrap(), dates);
        let read: Dates = serde_json::from_str(r#"{"at":"2022-01-01T00:00:00Z"}"#).unwrap();
        assert_eq!(read.at, dates.at);
    }

    #[test]
    fn explicit_nulls_and_empty_strings_read_as_none() {
        let ids: Ids = serde_json::from_str(
            r#"{"id":"00000000000000000000000000000001","parent":null,"children":[],"extra":null}"#,
        )
        .unwrap();
        assert_eq!((ids.parent, ids.extra), (None, None));
        // `JsonNullableGuidConverter` precedes the nullable-struct factory: "" is
        // NOT null for a Guid? — it is a parse error, as in Jellyfin.
        assert!(
            serde_json::from_str::<Ids>(r#"{"id":"00000000000000000000000000000001","parent":""}"#)
                .is_err()
        );
        let dates: Dates =
            serde_json::from_str(r#"{"at":"2022-01-01T00:00:00Z","maybe":null}"#).unwrap();
        assert_eq!(dates.maybe, None);
        let dates: Dates =
            serde_json::from_str(r#"{"at":"2022-01-01T00:00:00Z","maybe":""}"#).unwrap();
        assert_eq!(dates.maybe, None);
        assert!(
            serde_json::from_str::<Dates>(r#"{"at":"2022-01-01T00:00:00Z","maybe":"x"}"#).is_err()
        );
    }

    #[test]
    fn dates_read_every_form_utf8jsonreader_accepts() {
        let expect = Utc.with_ymd_and_hms(2022, 1, 1, 0, 0, 0).unwrap();
        for text in [
            "2022-01-01T00:00:00Z",
            "2022-01-01T00:00:00.0000000Z",
            "2022-01-01T01:00:00+01:00",
            "2022-01-01T00:00:00",
            "2022-01-01",
            " 2022-01-01 ",
        ] {
            assert_eq!(super::datetime::parse(text).unwrap(), expect, "{text}");
        }
        assert!(super::datetime::parse("yesterday").is_err());
        assert!(super::datetime::parse("").is_err());
    }
}
