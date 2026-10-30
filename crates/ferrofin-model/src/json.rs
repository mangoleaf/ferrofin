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

mod datetime_input;
pub mod enums;
pub mod number;
pub mod value;

/// `JsonGuidConverter` — `Uuid` fields.
pub mod guid {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    struct ReadGuid(Uuid);

    impl<'de> Deserialize<'de> for ReadGuid {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct GuidVisitor;
            impl serde::de::Visitor<'_> for GuidVisitor {
                type Value = ReadGuid;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a GUID string or null")
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<ReadGuid, E> {
                    Ok(ReadGuid(Uuid::nil()))
                }
                fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<ReadGuid, E> {
                    ferrofin_util::guid_extensions::parse_dotnet_guid(text)
                        .map(ReadGuid)
                        .ok_or_else(|| E::custom("invalid GUID"))
                }
            }
            // deserialize_any deliberately accepts actual strings only: the
            // string-member coercion must not turn numeric tokens into GUIDs.
            d.deserialize_any(GuidVisitor)
        }
    }

    /// Writes `ToString("N")`, from a stack buffer: nothing is allocated.
    ///
    /// # Errors
    ///
    /// Propagates the serializer's error.
    pub fn serialize<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.simple().encode_lower(&mut Uuid::encode_buffer()))
    }

    /// Reads any guid spelling; `null` is `Guid.Empty`.
    ///
    /// # Errors
    ///
    /// Fails when the value is neither a guid string nor `null`.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        ReadGuid::deserialize(deserializer).map(|guid| guid.0)
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
                Some(id) if !id.is_nil() => super::serialize(id, serializer),
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
            Option::<super::ReadGuid>::deserialize(deserializer)
                .map(|value| value.map(|guid| guid.0))
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
            let mut text = Uuid::encode_buffer();
            for id in value {
                seq.serialize_element(&*id.simple().encode_lower(&mut text))?;
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
            Vec::<super::ReadGuid>::deserialize(deserializer)
                .map(|values| values.into_iter().map(|guid| guid.0).collect())
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
            Option::<Vec<super::ReadGuid>>::deserialize(deserializer)
                .map(|values| values.map(|ids| ids.into_iter().map(|guid| guid.0).collect()))
        }
    }
}

/// `JsonDateTimeConverter` — `DateTime<Utc>` fields.
pub mod datetime {
    use std::borrow::Cow;

    use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
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
        WireText::new(value).as_str().into_owned()
    }

    /// Writes [`format`]'s text, from a stack buffer: nothing is allocated.
    ///
    /// # Errors
    ///
    /// Propagates the serializer's error.
    pub fn serialize<S: Serializer>(
        value: &DateTime<Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&WireText::new(value).as_str())
    }

    /// The longest [`format`] text: a signed six-digit year (chrono's widest,
    /// `-262143`), `-MM-ddTHH:mm:ss.`, eight tick digits (a leap second's
    /// fraction is 10^7 ticks or more) and `Z`.
    const WIRE_TEXT_LEN: usize = 7 + 16 + 8 + 1;

    /// [`format`]'s text, built on the stack: the bytes chrono's
    /// `%Y-%m-%dT%H:%M:%S` and two `format!`s wrote before, without their
    /// seven allocations per date.
    struct WireText {
        bytes: [u8; WIRE_TEXT_LEN],
        len: usize,
    }

    impl WireText {
        fn new(value: &DateTime<Utc>) -> Self {
            let mut text = Self {
                bytes: [0; WIRE_TEXT_LEN],
                len: 0,
            };
            // chrono's `%Y`: four digits for the years 0..=9999, otherwise a sign
            // and at least four digits.
            let year = value.year();
            if !(0..10_000).contains(&year) {
                text.push(if year < 0 { b'-' } else { b'+' });
            }
            text.push_digits(year.unsigned_abs(), 4);
            text.push(b'-');
            text.push_digits(value.month(), 2);
            text.push(b'-');
            text.push_digits(value.day(), 2);
            text.push(b'T');
            text.push_digits(value.hour(), 2);
            text.push(b':');
            text.push_digits(value.minute(), 2);
            text.push(b':');
            // A leap second is second 59 with 10^9 ns or more, which `%S` writes
            // as 60.
            let nanos = value.timestamp_subsec_nanos();
            text.push_digits(value.second() + nanos / 1_000_000_000, 2);
            text.push(b'.');
            let ticks = nanos / 100;
            let fraction = text.len;
            text.push_digits(ticks, 7);
            if ticks / 10_000 != 0 {
                // Milliseconds present: STJ trims the trailing zeros of the 7-digit
                // fraction (a millisecond is never all zeros here, so some digit stays).
                while text.len > fraction && text.bytes[text.len - 1] == b'0' {
                    text.len -= 1;
                }
            }
            text.push(b'Z');
            text
        }

        fn push(&mut self, byte: u8) {
            self.bytes[self.len] = byte;
            self.len += 1;
        }

        /// `value` in decimal, zero-padded to at least `width` digits
        /// (`{value:0width$}`).
        fn push_digits(&mut self, value: u32, width: usize) {
            let mut count = 1;
            let mut rest = value / 10;
            while rest != 0 {
                count += 1;
                rest /= 10;
            }
            let end = self.len + count.max(width);
            let mut rest = value;
            for slot in self.bytes[self.len..end].iter_mut().rev() {
                *slot = b"0123456789"[(rest % 10) as usize];
                rest /= 10;
            }
            self.len = end;
        }

        /// The text. Every byte is ASCII, so this always borrows.
        fn as_str(&self) -> Cow<'_, str> {
            String::from_utf8_lossy(&self.bytes[..self.len])
        }
    }

    /// Parses stored and query-string dates, including surrounding whitespace.
    /// JSON body converters use the stricter `Utf8JsonReader.GetDateTime`
    /// grammar; this helper retains the broader non-JSON input behavior.
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

    /// Reads the strict ISO-8601 grammar of `Utf8JsonReader.GetDateTime`.
    ///
    /// # Errors
    ///
    /// Fails when the value is not a string or not a timestamp.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<Utc>, D::Error> {
        let text = super::value::StringToken::deserialize(deserializer)?;
        super::datetime_input::parse(&text.0).map_err(serde::de::Error::custom)
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
                Some(dt) => super::serialize(dt, serializer),
                None => serializer.serialize_none(),
            }
        }

        /// Reads a strict ISO-8601 timestamp; `null`/absent — and `""`,
        /// which `JsonNullableStructConverter` reads as null because "some
        /// clients send an empty string" — is `None`.
        ///
        /// # Errors
        ///
        /// Fails when the value is neither a timestamp, `""` nor `null`.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<DateTime<Utc>>, D::Error> {
            match Option::<super::super::value::StringToken>::deserialize(deserializer)? {
                None => Ok(None),
                Some(text) if text.0.is_empty() => Ok(None),
                Some(text) => super::super::datetime_input::parse(&text.0)
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

    /// `at` written through both date attributes.
    fn both_fields(at: DateTime<Utc>) -> String {
        serde_json::to_string(&Dates {
            at,
            maybe: Some(at),
        })
        .unwrap()
    }

    #[test]
    fn date_edge_values_keep_their_wire_text() {
        let second = Utc.with_ymd_and_hms(2026, 8, 23, 16, 57, 22).unwrap();
        let ns = chrono::Duration::nanoseconds;
        for (at, text) in [
            // `DateTime.MinValue` and `DateTime.MaxValue`.
            (
                super::datetime::dotnet_min(),
                "0001-01-01T00:00:00.0000000Z",
            ),
            (
                Utc.with_ymd_and_hms(9999, 12, 31, 23, 59, 59).unwrap() + ns(999_999_900),
                "9999-12-31T23:59:59.9999999Z",
            ),
            // The largest tick count below a millisecond keeps all seven digits …
            (second + ns(999_900), "2026-08-23T16:57:22.0009999Z"),
            (second + ns(100), "2026-08-23T16:57:22.0000001Z"),
            // … and the first millisecond is trimmed.
            (second + ns(1_000_000), "2026-08-23T16:57:22.001Z"),
            (second + ns(123_456_700), "2026-08-23T16:57:22.1234567Z"),
            (second + ns(500_000_000), "2026-08-23T16:57:22.5Z"),
            // Nanoseconds below the 100 ns tick are dropped.
            (second + ns(99), "2026-08-23T16:57:22.0000000Z"),
        ] {
            assert_eq!(super::datetime::format(&at), text);
            assert_eq!(
                both_fields(at),
                format!(r#"{{"at":"{text}","maybe":"{text}"}}"#)
            );
        }
    }

    /// The text `datetime::format` wrote before it moved to a stack buffer:
    /// chrono's `%Y-%m-%dT%H:%M:%S`, then the tick fraction.
    fn chrono_format(value: &DateTime<Utc>) -> String {
        let ticks = value.timestamp_subsec_nanos() / 100;
        let mut fraction = format!("{ticks:07}");
        if ticks / 10_000 != 0 {
            while fraction.ends_with('0') {
                fraction.pop();
            }
        }
        format!("{}.{fraction}Z", value.format("%Y-%m-%dT%H:%M:%S"))
    }

    #[test]
    fn dates_are_written_exactly_as_chrono_formats_them() {
        let years = [
            -262_143, -10_000, -9_999, -1, 0, 1, 9, 99, 999, 1_000, 1_970, 2_026, 9_999, 10_000,
            262_142,
        ];
        let nanos = [
            0,
            99,
            100,
            900,
            99_900,
            999_999,
            1_000_000,
            1_000_100,
            10_000_000,
            120_000_000,
            123_456_700,
            479_586_000,
            500_000_000,
            764_472_999,
            999_000_000,
            999_999_900,
            999_999_999,
        ];
        // A leap second is second 59 with 10^9 ns or more; `%S` writes it as 60.
        let leap = [1_000_000_000, 1_000_000_100, 1_500_000_000, 1_999_999_999];
        let mut checked = 0;
        for year in years {
            for (time, subsec) in nanos
                .iter()
                .map(|&n| ((1, 2, 3), n))
                .chain(leap.iter().map(|&n| ((23, 59, 59), n)))
            {
                let date = chrono::NaiveDate::from_ymd_opt(year, 12, 31).unwrap();
                let at = date
                    .and_hms_nano_opt(time.0, time.1, time.2, subsec)
                    .unwrap()
                    .and_utc();
                let text = chrono_format(&at);
                assert_eq!(super::datetime::format(&at), text, "{at:?}");
                assert_eq!(
                    both_fields(at),
                    format!(r#"{{"at":"{text}","maybe":"{text}"}}"#),
                    "{at:?}"
                );
                checked += 1;
            }
        }
        // chrono's extremes, whatever their year.
        for at in [DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC] {
            assert_eq!(super::datetime::format(&at), chrono_format(&at), "{at:?}");
            checked += 1;
        }
        assert_eq!(checked, years.len() * (nanos.len() + leap.len()) + 2);
    }

    #[test]
    fn guids_are_written_exactly_as_their_simple_display() {
        for id in [
            Uuid::nil(),
            Uuid::max(),
            ID,
            Uuid::from_u128(1),
            Uuid::from_u128(0xabcd_ef00_0000_0000_0000_0000_0000_0000),
        ] {
            let n = id.simple().to_string();
            let nullable = if id.is_nil() {
                "null".to_owned()
            } else {
                format!(r#""{n}""#)
            };
            assert_eq!(
                serde_json::to_string(&Ids {
                    id,
                    parent: Some(id),
                    children: vec![id, id],
                    extra: Some(vec![id]),
                })
                .unwrap(),
                format!(
                    r#"{{"id":"{n}","parent":{nullable},"children":["{n}","{n}"],"extra":["{n}"]}}"#
                )
            );
        }
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
