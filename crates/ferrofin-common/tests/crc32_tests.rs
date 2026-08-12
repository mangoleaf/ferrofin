//! Transliteration of `Jellyfin.Common.Tests/Crc32Tests.cs`.

use ferrofin_common::crc32;
use rstest::rstest;

/// Parses an UPPERCASE/lowercase hex string (test helper mirroring
/// `Convert.FromHexString`).
fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex in test data"))
        .collect()
}

#[test]
fn compute_empty_zero() {
    assert_eq!(0u32, crc32::compute(&[]));
}

#[rstest]
#[case(0x414f_a339, "The quick brown fox jumps over the lazy dog")]
fn compute_valid_success(#[case] expected: u32, #[case] data: &str) {
    assert_eq!(expected, crc32::compute(data.as_bytes()));
}

#[rstest]
#[case(
    0x414f_a339,
    "54686520717569636B2062726F776E20666F78206A756D7073206F76657220746865206C617A7920646F67"
)]
#[case(
    0x190a_55ad,
    "0000000000000000000000000000000000000000000000000000000000000000"
)]
#[case(
    0xff6c_ab0b,
    "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"
)]
#[case(
    0x9126_7e8a,
    "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F"
)]
fn compute_valid_hex_success(#[case] expected: u32, #[case] data: &str) {
    assert_eq!(expected, crc32::compute(&from_hex(data)));
}
