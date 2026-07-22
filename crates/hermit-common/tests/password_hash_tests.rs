//! Transliteration of `Jellyfin.Model.Tests/Cryptography/PasswordHashTests.cs`.

use hermit_common::CryptoError;
use hermit_common::cryptography::PasswordHash;
use rstest::rstest;

/// Parses UPPERCASE hex (test helper mirroring `Convert.FromHexString`).
fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex in test data"))
        .collect()
}

/// Builds a `(key, value)` parameter pair.
fn param(k: &str, v: &str) -> (String, String) {
    (k.to_owned(), v.to_owned())
}

const HASH_HEX: &str = "62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D";

#[test]
fn ctor_empty_throws_argument_exception() {
    // C# distinguishes null (ArgumentNullException) from empty (ArgumentException);
    // Rust has no null string, so both collapse to the empty-string argument error.
    assert!(matches!(
        PasswordHash::with_hash("", Vec::new()),
        Err(CryptoError::Argument(_))
    ));
}

/// The `Parse_Valid_TestData` fixture: (input, expected).
fn parse_valid_test_data() -> Vec<(&'static str, PasswordHash)> {
    vec![
        // Id
        (
            "$PBKDF2",
            PasswordHash::with_hash("PBKDF2", Vec::new()).unwrap(),
        ),
        // Id + parameter
        (
            "$PBKDF2$iterations=1000",
            PasswordHash::new(
                "PBKDF2",
                Vec::new(),
                Vec::new(),
                vec![param("iterations", "1000")],
            )
            .unwrap(),
        ),
        // Id + parameters
        (
            "$PBKDF2$iterations=1000,m=120",
            PasswordHash::new(
                "PBKDF2",
                Vec::new(),
                Vec::new(),
                vec![param("iterations", "1000"), param("m", "120")],
            )
            .unwrap(),
        ),
        // Id + hash
        (
            "$PBKDF2$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
            PasswordHash::new("PBKDF2", from_hex(HASH_HEX), Vec::new(), Vec::new()).unwrap(),
        ),
        // Id + salt + hash
        (
            "$PBKDF2$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
            PasswordHash::new("PBKDF2", from_hex(HASH_HEX), from_hex("69F420"), Vec::new())
                .unwrap(),
        ),
        // Id + parameter + hash
        (
            "$PBKDF2$iterations=1000$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
            PasswordHash::new(
                "PBKDF2",
                from_hex(HASH_HEX),
                Vec::new(),
                vec![param("iterations", "1000")],
            )
            .unwrap(),
        ),
        // Id + parameters + hash
        (
            "$PBKDF2$iterations=1000,m=120$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
            PasswordHash::new(
                "PBKDF2",
                from_hex(HASH_HEX),
                Vec::new(),
                vec![param("iterations", "1000"), param("m", "120")],
            )
            .unwrap(),
        ),
        // Id + parameters + salt + hash
        (
            "$PBKDF2$iterations=1000,m=120$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
            PasswordHash::new(
                "PBKDF2",
                from_hex(HASH_HEX),
                from_hex("69F420"),
                vec![param("iterations", "1000"), param("m", "120")],
            )
            .unwrap(),
        ),
    ]
}

#[test]
fn parse_valid_success() {
    for (password_hash_string, expected) in parse_valid_test_data() {
        let password_hash = PasswordHash::parse(password_hash_string).unwrap();
        assert_eq!(expected.id(), password_hash.id());
        assert_eq!(expected.parameters(), password_hash.parameters());
        assert_eq!(expected.salt(), password_hash.salt());
        assert_eq!(expected.hash(), password_hash.hash());
        assert_eq!(expected.to_string(), password_hash.to_string());
    }
}

#[rstest]
#[case("$PBKDF2")]
#[case("$PBKDF2$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")]
#[case("$PBKDF2$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")]
#[case("$PBKDF2$iterations=1000$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")]
#[case(
    "$PBKDF2$iterations=1000,m=120$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
)]
#[case(
    "$PBKDF2$iterations=1000,m=120$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
)]
#[case("$PBKDF2$iterations=1000,m=120")]
fn to_string_roundtrip_success(#[case] password_hash: &str) {
    assert_eq!(
        password_hash,
        PasswordHash::parse(password_hash).unwrap().to_string()
    );
}

#[test]
fn parse_empty_throws_argument_exception() {
    assert!(matches!(
        PasswordHash::parse(""),
        Err(CryptoError::Argument(_))
    ));
}

#[rstest]
#[case("$")] // No id
#[case("$$")] // Empty segments
#[case("PBKDF2$")] // Doesn't start with $
#[case("$PBKDF2$$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Empty segment
#[case("$PBKDF2$iterations=1000$$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Empty salt segment
#[case("$PBKDF2$iterations=1000$69F420$")] // Empty hash segment
#[case("$PBKDF2$=$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Invalid parameter
#[case("$PBKDF2$=1000$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Invalid parameter
#[case("$PBKDF2$iterations=$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Invalid parameter
#[case("$PBKDF2$iterations=1000$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D$")] // Ends on $
#[case(
    "$PBKDF2$iterations=1000$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D$"
)] // Extra segment
#[case(
    "$PBKDF2$iterations=1000$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D$anotherone"
)] // Extra segment
#[case(
    "$PBKDF2$iterations=1000$invalidstalt$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
)] // Invalid salt
#[case("$PBKDF2$iterations=1000$69F420$invalid hash")] // Invalid hash
#[case("$PBKDF2$69F420$")] // Empty hash
fn parse_invalid_format_throws_format_exception(#[case] password_hash: &str) {
    assert!(matches!(
        PasswordHash::parse(password_hash),
        Err(CryptoError::Format(_))
    ));
}
