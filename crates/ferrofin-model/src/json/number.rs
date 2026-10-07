//! `System.Text.Json`'s typed numeric parsing rules, shared by request binding
//! and the few model readers that also consume quoted-number profile files.
//!
//! Measured against Jellyfin 12.2 `JsonDefaults.Options`: signed integers allow
//! a leading plus in strings; unsigned integers do not. No type trims spaces.
//! Quoted floats allow exactly `NaN`, `Infinity`, and `-Infinity`, but reject
//! overflow; bare JSON floating-point numbers may overflow to infinity.

/// A primitive number accepted by Jellyfin's JSON number handling.
///
/// Implementations cover Rust's primitive integer and floating-point types.
pub trait JsonNumber: Sized {
    /// Parses an already decoded string or validated JSON number token.
    /// `quoted` distinguishes the floating-point overflow rules.
    fn parse_json_number(text: &str, quoted: bool) -> Option<Self>;
}

macro_rules! integers {
    ($($ty:ty => $signed:literal),* $(,)?) => {$ (
        impl JsonNumber for $ty {
            fn parse_json_number(text: &str, _quoted: bool) -> Option<Self> {
                let digits = if $signed {
                    text.strip_prefix(['+', '-']).unwrap_or(text)
                } else {
                    text
                };
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                text.parse().ok()
            }
        }
    )* };
}
integers! {
    i8 => true, i16 => true, i32 => true, i64 => true, i128 => true,
    u8 => false, u16 => false, u32 => false, u64 => false, u128 => false,
}

macro_rules! floats {
    ($($ty:ty),* $(,)?) => {$ (
        impl JsonNumber for $ty {
            fn parse_json_number(text: &str, quoted: bool) -> Option<Self> {
                if quoted {
                    match text {
                        "NaN" => return Some(Self::NAN),
                        "Infinity" => return Some(Self::INFINITY),
                        "-Infinity" => return Some(Self::NEG_INFINITY),
                        _ => {}
                    }
                }
                // Rust also accepts spellings such as `inf` and `nan` that
                // Utf8JsonReader.GetDoubleWithQuotes refuses.
                if !text.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) {
                    return None;
                }
                let value: Self = text.parse().ok()?;
                (!quoted || value.is_finite()).then_some(value)
            }
        }
    )* };
}
floats!(f32, f64);

/// Identifies legacy device-profile integers to the request binder, which
/// enforces numeric JSON rules before the file reader sees the value.
pub const PROFILE_I32: &str = "$ferrofin::profile_i32";

/// Reads integers in device-profile files, including their legacy empty-string
/// zero. Request deserializers intercept the marker and apply the stricter
/// non-nullable number rules, so an empty HTTP value is still an error.
///
/// # Errors
/// Rejects non-integer tokens, overflow, whitespace, and invalid number text.
pub fn profile_i32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    struct Profile;
    impl<'de> serde::de::Visitor<'de> for Profile {
        type Value = i32;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a device-profile integer")
        }
        fn visit_newtype_struct<D: serde::Deserializer<'de>>(self, d: D) -> Result<i32, D::Error> {
            d.deserialize_any(self)
        }
        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<i32, E> {
            i32::try_from(value).map_err(E::custom)
        }
        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<i32, E> {
            i32::try_from(value).map_err(E::custom)
        }
        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<i32, E> {
            if text.is_empty() {
                return Ok(0);
            }
            i32::parse_json_number(text, true)
                .ok_or_else(|| E::custom("invalid device-profile integer"))
        }
    }
    d.deserialize_newtype_struct(PROFILE_I32, Profile)
}

#[cfg(test)]
mod tests {
    use super::JsonNumber;
    use rstest::rstest;

    #[rstest]
    #[case("5", Some(5))]
    #[case("+5", Some(5))]
    #[case("005", Some(5))]
    #[case("-0", Some(0))]
    #[case(" 5", None)]
    #[case("5 ", None)]
    #[case("", None)]
    #[case("5.0", None)]
    #[case("5e0", None)]
    #[case("2147483648", None)]
    fn signed_integer(#[case] input: &str, #[case] expected: Option<i32>) {
        assert_eq!(i32::parse_json_number(input, true), expected);
    }

    #[rstest]
    #[case("0", Some(0))]
    #[case("255", Some(255))]
    #[case("+1", None)]
    #[case("-0", None)]
    #[case("256", None)]
    fn unsigned_integer(#[case] input: &str, #[case] expected: Option<u8>) {
        assert_eq!(u8::parse_json_number(input, true), expected);
    }

    #[rstest]
    #[case("1.5", true, Some(1.5))]
    #[case(".5", true, Some(0.5))]
    #[case("1.", true, Some(1.0))]
    #[case("1e2", true, Some(100.0))]
    #[case("1e400", true, None)]
    #[case("1e400", false, Some(f64::INFINITY))]
    #[case("Infinity", true, Some(f64::INFINITY))]
    #[case("-Infinity", true, Some(f64::NEG_INFINITY))]
    #[case("inf", true, None)]
    #[case("nan", true, None)]
    #[case("+Infinity", true, None)]
    #[case(" 1", true, None)]
    fn floating_point(#[case] input: &str, #[case] quoted: bool, #[case] expected: Option<f64>) {
        assert_eq!(f64::parse_json_number(input, quoted), expected);
    }

    #[test]
    fn named_nan_and_single_precision_overflow() {
        assert!(f32::parse_json_number("NaN", true).unwrap().is_nan());
        assert!(f64::parse_json_number("NaN", true).unwrap().is_nan());
        assert_eq!(f32::parse_json_number("1e40", true), None);
        assert_eq!(f32::parse_json_number("1e40", false), Some(f32::INFINITY));
        assert_eq!(f64::parse_json_number("1e40", true), Some(1e40));
    }
    #[test]
    fn quoted_numbers_match_jellyfin_oracle() {
        let fixture =
            include_str!("../../../ferrofin-api/tests/data/json-binding/jellyfin-12.2.jsonl");
        let mut checked = 0;
        for line in fixture.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = row["input"].as_str().unwrap();
            let Ok(text) = serde_json::from_str::<String>(input) else {
                continue;
            };
            macro_rules! check {
                ($ty:ty) => {{
                    let actual = <$ty>::parse_json_number(&text, true);
                    assert_eq!(
                        actual.is_some(),
                        row["accepted"].as_bool().unwrap(),
                        "{} from {input}",
                        stringify!($ty)
                    );
                    if let Some(actual) = actual {
                        let expected = row["value"].as_str().unwrap();
                        // Display the special floats using Rust's spelling;
                        // all finite values compare in the destination type.
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
        assert_eq!(checked, 700);
    }
}
