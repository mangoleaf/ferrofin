//! What .NET's code pages hold that `encoding_rs`'s tables lack (gl-2000).
//!
//! Jellyfin decodes a legacy-encoded subtitle or playlist with .NET's code
//! pages (`System.Text.Encoding.CodePages`), and Ferrofin with `encoding_rs`,
//! whose tables are the WHATWG Encoding Standard's. [`decode`] reads two
//! things as .NET does:
//! - the user-defined rows of code page 949, which the WHATWG index leaves
//!   out ([`user_defined`]);
//! - the GB18030 sequence 81 35 F4 37, which is `ḿ` in .NET's table and a
//!   Private Use Area code point in the WHATWG one.
//!
//! The oracle is Jellyfin 12.4 itself: sidecar subtitles holding every byte
//! pair of each charset, served by a stock server and by Ferrofin, and the
//! text compared pair by pair. The `escape-charsets` parity row lists what
//! else that comparison found.

use encoding_rs::{DecoderResult, Encoding};

/// The first code point of the Private Use Area, where code page 949 puts
/// its user-defined characters.
const PRIVATE_USE_START: u32 = 0xE000;

/// The cells of a row of a 94 by 94 character set.
const ROW_CELLS: u8 = 94;

/// What `encoding`'s .NET code page holds for the pair `lead`, `trail` in a
/// user-defined row that the WHATWG index leaves out.
///
/// Code page 949, .NET's table for the bytes [`encoding_rs::EUC_KR`] decodes,
/// maps its rows C9 and FE (trail bytes A1 to FE) into the Private Use Area:
/// C9A1 is U+E000, FEA1 is U+E05E and FEFE is U+E0BB. Measured on Jellyfin
/// 12.4, in a `cp949` subtitle and in an ISO-2022-KR one (code page 50225,
/// which reads 949's table).
#[must_use]
pub fn user_defined(encoding: &'static Encoding, lead: u8, trail: u8) -> Option<char> {
    if encoding != encoding_rs::EUC_KR {
        return None;
    }
    let row = match lead {
        0xC9 => 0,
        0xFE => 1,
        _ => return None,
    };
    let cell = trail.checked_sub(0xA1).filter(|cell| *cell < ROW_CELLS)?;
    char::from_u32(PRIVATE_USE_START + row * u32::from(ROW_CELLS) + u32::from(cell))
}

/// `bytes` decoded from `encoding` as [`Encoding::decode`] decodes them, but
/// for what .NET reads otherwise:
/// - in Korean text, the pairs [`user_defined`] knows, where `encoding_rs`
///   alone answers U+FFFD;
/// - in Simplified Chinese text, 81 35 F4 37, which .NET's GB18030 decodes
///   to U+1E3F (`ḿ`) and `encoding_rs` to U+E7C7. No other sequence decodes
///   to U+E7C7 (every two-byte pair and four-byte sequence of the Basic
///   Multilingual Plane was decoded to check), so that character is replaced.
///
/// A byte order mark is not looked for in Korean text: callers rule one out
/// before they detect a charset.
#[must_use]
pub fn decode(encoding: &'static Encoding, bytes: &[u8]) -> String {
    if encoding == encoding_rs::GBK {
        return encoding.decode(bytes).0.replace('\u{E7C7}', "\u{1E3F}");
    }
    if encoding != encoding_rs::EUC_KR {
        return encoding.decode(bytes).0.into_owned();
    }
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut out = String::new();
    let mut rest = bytes;
    loop {
        out.reserve(
            decoder
                .max_utf8_buffer_length_without_replacement(rest.len())
                .unwrap_or(rest.len()),
        );
        let (result, read) = decoder.decode_to_string_without_replacement(rest, &mut out, true);
        match result {
            DecoderResult::InputEmpty => return out,
            DecoderResult::OutputFull => {}
            DecoderResult::Malformed(length, after) => {
                // The sequence ends `after` bytes before the last byte read.
                let end = read.saturating_sub(usize::from(after));
                let sequence = rest.get(end.saturating_sub(usize::from(length))..end);
                out.push(match sequence {
                    Some(&[lead, trail]) => {
                        user_defined(encoding, lead, trail).unwrap_or(char::REPLACEMENT_CHARACTER)
                    }
                    _ => char::REPLACEMENT_CHARACTER,
                });
            }
        }
        rest = rest.get(read..).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, user_defined};

    /// The corners of both rows, as Jellyfin 12.4 serves them from a `cp949`
    /// sidecar, and their neighbours, which code page 949 does not map.
    #[rstest::rstest]
    #[case(0xC9, 0xA1, Some('\u{E000}'))]
    #[case(0xC9, 0xFE, Some('\u{E05D}'))]
    #[case(0xFE, 0xA1, Some('\u{E05E}'))]
    #[case(0xFE, 0xFE, Some('\u{E0BB}'))]
    #[case(0xC9, 0xA0, None)]
    #[case(0xC9, 0xFF, None)]
    #[case(0xFE, 0x41, None)]
    #[case(0xC8, 0xA1, None)]
    #[case(0xCA, 0xA1, None)]
    #[case(0xFD, 0xFE, None)]
    fn code_page_949_maps_its_user_defined_rows_into_the_private_use_area(
        #[case] lead: u8,
        #[case] trail: u8,
        #[case] expected: Option<char>,
    ) {
        assert_eq!(user_defined(encoding_rs::EUC_KR, lead, trail), expected);
    }

    /// Only code page 949 has the rows: GBK and Big5 map C9A1 themselves.
    #[test]
    fn other_charsets_have_no_user_defined_rows() {
        for encoding in [encoding_rs::GBK, encoding_rs::BIG5, encoding_rs::SHIFT_JIS] {
            assert_eq!(
                user_defined(encoding, 0xC9, 0xA1),
                None,
                "{}",
                encoding.name()
            );
        }
    }

    /// Every pair after `한` and before `a`: the two rows are the Private Use
    /// Area, and everything else is what `encoding_rs` answers, U+FFFD for
    /// what it cannot decode and an ASCII trail byte read again.
    #[test]
    fn korean_text_differs_from_encoding_rs_only_in_the_user_defined_rows() {
        for lead in 0x80..=0xFF_u8 {
            for trail in 0x00..=0xFF_u8 {
                let bytes = [0xC7, 0xD1, lead, trail, b'a'];
                let expected = match user_defined(encoding_rs::EUC_KR, lead, trail) {
                    Some(character) => format!("한{character}a"),
                    None => encoding_rs::EUC_KR.decode(&bytes).0.into_owned(),
                };
                assert_eq!(
                    decode(encoding_rs::EUC_KR, &bytes),
                    expected,
                    "{lead:02X}{trail:02X}"
                );
            }
        }
    }

    #[test]
    fn a_lead_byte_that_ends_the_text_is_replaced() {
        assert_eq!(decode(encoding_rs::EUC_KR, b"\xc7\xd1\xc9"), "한\u{FFFD}");
        assert_eq!(decode(encoding_rs::EUC_KR, b""), "");
    }

    /// 81 35 F4 37 is `ḿ` in .NET's GB18030, as Jellyfin 12.4 serves it from
    /// a `gb18030` sidecar; `encoding_rs` alone answers U+E7C7.
    #[test]
    fn the_four_byte_m_acute_of_gb18030_is_read_as_dotnet_reads_it() {
        assert_eq!(
            decode(encoding_rs::GBK, b"\xd6\xd0\xce\xc4 \x81\x35\xf4\x37"),
            "中文 \u{1E3F}"
        );
    }

    /// The replacement is exact while 81 35 F4 37 is the only sequence
    /// `encoding_rs` decodes to U+E7C7: every two-byte pair, and every
    /// four-byte sequence of the Basic Multilingual Plane.
    #[test]
    fn only_one_gb18030_sequence_decodes_to_the_replaced_code_point() {
        let mut sources = Vec::new();
        let mut note = |bytes: &[u8]| {
            if encoding_rs::GBK.decode(bytes).0.contains('\u{E7C7}') {
                sources.push(bytes.to_vec());
            }
        };
        for lead in 0x81..=0xFE_u8 {
            for trail in 0x40..=0xFE_u8 {
                note(&[lead, trail]);
            }
        }
        for first in 0x81..=0x84_u8 {
            for second in b'0'..=b'9' {
                for third in 0x81..=0xFE_u8 {
                    for fourth in b'0'..=b'9' {
                        note(&[first, second, third, fourth]);
                    }
                }
            }
        }
        assert_eq!(sources, [b"\x81\x35\xf4\x37".to_vec()]);
    }

    /// Other charsets go through [`encoding_rs::Encoding::decode`] untouched,
    /// byte order mark handling included.
    #[test]
    fn other_charsets_decode_as_encoding_rs_does() {
        assert_eq!(
            decode(encoding_rs::GBK, b"\xd6\xd0\xce\xc4\xc9\xa1"),
            "中文伞"
        );
        assert_eq!(
            decode(encoding_rs::WINDOWS_1252, b"caf\xe9 \x81"),
            "café \u{81}"
        );
        assert_eq!(decode(encoding_rs::UTF_16LE, b"\xff\xfea\x00"), "a");
    }
}
