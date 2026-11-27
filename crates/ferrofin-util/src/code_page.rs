//! What .NET's code pages read otherwise than `encoding_rs` (gl-2000, gl-2010).
//!
//! Jellyfin decodes a legacy-encoded subtitle or playlist with .NET's code
//! pages (`System.Text.Encoding.CodePages`), and Ferrofin with `encoding_rs`,
//! whose tables and error handling are the WHATWG Encoding Standard's.
//! [`decode`] reads three things as .NET does:
//! - the user-defined rows of code page 949, which the WHATWG index leaves
//!   out ([`user_defined`]);
//! - the GB18030 sequence 81 35 F4 37, which is `ḿ` in .NET's table and a
//!   Private Use Area code point in the WHATWG one;
//! - bytes with no character in their charset: .NET writes `?` (U+30FB in the
//!   Japanese code pages, a code point of its own for a lone byte), and its
//!   double-byte decoders take the byte after a lead byte with it, where
//!   `encoding_rs` writes U+FFFD and reads an ASCII byte again.
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

/// A run of lone bytes, first and last, and the code point .NET's code page
/// gives the first of them. The bytes after it take the code points after it.
type LoneBytes = (u8, u8, u32);

/// The bytes `encoding_rs` has no character for and `encoding`'s .NET code
/// page reads alone, as Jellyfin 12.4 serves them (gl-2010): from sidecars it
/// named `cp949`, `big5`, `shift-jis` and `euc-jp`, and for the single-byte
/// code pages from a `ComicInfo.xml` declaring each one. Code page 51932
/// (`euc-jp`) reads 932's table. All but the three U+0080 are Private Use
/// Area code points.
///
/// Empty for an encoding [`decode`] leaves to `encoding_rs`.
fn lone_bytes(encoding: &'static Encoding) -> &'static [LoneBytes] {
    if encoding == encoding_rs::EUC_KR {
        &[(0x80, 0x80, 0x80), (0xFF, 0xFF, 0xF8F7)]
    } else if encoding == encoding_rs::BIG5 {
        &[(0x80, 0x80, 0x80), (0xFF, 0xFF, 0xF8F8)]
    } else if encoding == encoding_rs::SHIFT_JIS {
        &[(0xA0, 0xA0, 0xF8F0), (0xFD, 0xFF, 0xF8F1)]
    } else if encoding == encoding_rs::EUC_JP {
        &[
            (0x80, 0x80, 0x80),
            (0xA0, 0xA0, 0xF8F0),
            (0xFF, 0xFF, 0xF8F3),
        ]
    } else if encoding == encoding_rs::WINDOWS_1253 {
        &[
            (0xAA, 0xAA, 0xF8F9),
            (0xD2, 0xD2, 0xF8FA),
            (0xFF, 0xFF, 0xF8FB),
        ]
    } else if encoding == encoding_rs::WINDOWS_1255 {
        &[
            (0xD9, 0xDF, 0xF88D),
            (0xFB, 0xFC, 0xF894),
            (0xFF, 0xFF, 0xF896),
        ]
    } else if encoding == encoding_rs::WINDOWS_1257 {
        &[(0xA1, 0xA1, 0xF8FC), (0xA5, 0xA5, 0xF8FD)]
    } else if encoding == encoding_rs::WINDOWS_874 {
        &[(0xDB, 0xDE, 0xF8C1), (0xFC, 0xFF, 0xF8C5)]
    } else if encoding == encoding_rs::ISO_8859_6 {
        &[
            (0xA1, 0xA3, 0xF7C8),
            (0xA5, 0xAB, 0xF7CB),
            (0xAE, 0xBA, 0xF7D2),
            (0xBC, 0xBE, 0xF7DF),
            (0xC0, 0xC0, 0xF7E2),
            (0xDB, 0xDF, 0xF7E3),
            (0xF3, 0xFF, 0xF7E8),
        ]
    } else if encoding == encoding_rs::ISO_8859_7 {
        &[
            (0xAE, 0xAE, 0xF7C5),
            (0xD2, 0xD2, 0xF7C6),
            (0xFF, 0xFF, 0xF7C7),
        ]
    } else if encoding == encoding_rs::ISO_8859_8 {
        &[
            (0xA1, 0xA1, 0xF79C),
            (0xBF, 0xDE, 0xF79D),
            (0xFB, 0xFC, 0xF7BD),
            (0xFF, 0xFF, 0xF7C1),
        ]
    } else {
        &[]
    }
}

/// Whether `encoding`'s .NET decoder reads `byte` as the first of two. Code
/// page 51932 has no three-byte form, so 8F is no lead byte there.
fn is_lead_byte(encoding: &'static Encoding, byte: u8) -> bool {
    if encoding == encoding_rs::EUC_KR || encoding == encoding_rs::BIG5 {
        matches!(byte, 0x81..=0xFE)
    } else if encoding == encoding_rs::SHIFT_JIS {
        matches!(byte, 0x81..=0x9F | 0xE0..=0xFC)
    } else if encoding == encoding_rs::EUC_JP {
        matches!(byte, 0x8E | 0xA1..=0xFE)
    } else {
        false
    }
}

/// What `encoding`'s .NET decoder writes for the bytes `tail` starts with,
/// which `encoding_rs` cannot decode, and how many of them it takes. `None`
/// leaves the sequence to `encoding_rs`.
///
/// Measured on Jellyfin 12.4 (gl-2010), every high byte before every other
/// byte and at the end of the text, and as .NET 10's `GetChars` reads:
/// - `GB18030Encoding` writes `?` for the first byte alone, and for the four
///   bytes of a well-formed sequence that has no character;
/// - `DBCSCodePageEncoding` (code pages 949, 950, 932 and 51932) writes one
///   character for a lead byte and the byte after it, whatever that byte is
///   (a line feed too), and for a lead byte the text ends in;
/// - a byte that is no lead byte is what [`lone_bytes`] holds.
///
/// The character is `?`, and code page 932's default character U+30FB for
/// `shift-jis` and `euc-jp`.
fn dotnet_fallback(encoding: &'static Encoding, tail: &[u8]) -> Option<(char, usize)> {
    let (&first, rest) = tail.split_first()?;
    if encoding == encoding_rs::GBK {
        let four_bytes = matches!(
            tail,
            [0x81..=0xFE, b'0'..=b'9', 0x81..=0xFE, b'0'..=b'9', ..]
        );
        return Some(('?', if four_bytes { 4 } else { 1 }));
    }
    let replacement = if encoding == encoding_rs::SHIFT_JIS || encoding == encoding_rs::EUC_JP {
        '\u{30FB}'
    } else {
        '?'
    };
    if !is_lead_byte(encoding, first) {
        let lone = lone_bytes(encoding)
            .iter()
            .find(|(start, end, _)| (*start..=*end).contains(&first))
            .and_then(|(start, _, code)| char::from_u32(code + u32::from(first - start)));
        return Some((lone.unwrap_or(replacement), 1));
    }
    let Some(&second) = rest.first() else {
        return Some((replacement, 1));
    };
    if let Some(character) = user_defined(encoding, first, second) {
        return Some((character, 2));
    }
    // Code page 950 has a Private Use Area code point for every pair with a
    // trail byte but A3C0 to A3E0 and A3E2 to A3FE. Those pairs stay U+FFFD
    // until gl-2012 decides what Big5's user-defined ranges read as.
    if encoding == encoding_rs::BIG5
        && matches!(second, 0x40..=0x7E | 0xA1..=0xFE)
        && !(first == 0xA3 && second >= 0xC0 && second != 0xE1)
    {
        return None;
    }
    Some((replacement, 2))
}

/// `bytes` decoded from `encoding` as [`Encoding::decode`] decodes them, but
/// for what .NET reads otherwise:
/// - in Korean text, the pairs [`user_defined`] knows, where `encoding_rs`
///   alone answers U+FFFD;
/// - in Simplified Chinese text, 81 35 F4 37, which .NET's GB18030 decodes
///   to U+1E3F (`ḿ`) and `encoding_rs` to U+E7C7. No other sequence decodes
///   to U+E7C7 (every two-byte pair and four-byte sequence of the Basic
///   Multilingual Plane was decoded to check), so that character is replaced;
/// - in Korean, Chinese and Japanese text and in the single-byte code pages
///   with unassigned bytes, a sequence `encoding_rs` cannot decode, which is
///   what `dotnet_fallback` says in place of U+FFFD. The decoder goes on
///   after the bytes .NET takes, so a stray lead byte takes the character
///   after it with it.
///
/// A byte order mark is not looked for in those charsets: callers rule one
/// out before they detect a charset.
#[must_use]
pub fn decode(encoding: &'static Encoding, bytes: &[u8]) -> String {
    if encoding != encoding_rs::GBK && lone_bytes(encoding).is_empty() {
        return encoding.decode(bytes).0.into_owned();
    }
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut out = String::new();
    let mut position = 0;
    loop {
        let rest = bytes.get(position..).unwrap_or_default();
        out.reserve(
            decoder
                .max_utf8_buffer_length_without_replacement(rest.len())
                .unwrap_or(rest.len()),
        );
        let (result, read) = decoder.decode_to_string_without_replacement(rest, &mut out, true);
        position += read;
        match result {
            DecoderResult::InputEmpty => break,
            DecoderResult::OutputFull => {}
            DecoderResult::Malformed(length, after) => {
                // The sequence ends `after` bytes before the last byte read.
                let end = position.saturating_sub(usize::from(after));
                let start = end.saturating_sub(usize::from(length));
                let (character, taken) =
                    dotnet_fallback(encoding, bytes.get(start..).unwrap_or_default())
                        .unwrap_or((char::REPLACEMENT_CHARACTER, end - start));
                out.push(character);
                // .NET's decoder holds no bytes back: it reads on from here.
                position = start + taken.max(1);
                decoder = encoding.new_decoder_without_bom_handling();
            }
        }
    }
    if encoding == encoding_rs::GBK {
        out.replace('\u{E7C7}', "\u{1E3F}")
    } else {
        out
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

    /// Every pair of the two rows comes out of [`decode`] as the character
    /// [`user_defined`] holds, and the byte after it is read on its own.
    #[test]
    fn korean_text_keeps_every_user_defined_pair() {
        for lead in [0xC9, 0xFE_u8] {
            for trail in 0xA1..=0xFE_u8 {
                let character = user_defined(encoding_rs::EUC_KR, lead, trail).unwrap();
                assert_eq!(
                    decode(encoding_rs::EUC_KR, &[0xC7, 0xD1, lead, trail, b'a']),
                    format!("한{character}a"),
                    "{lead:02X}{trail:02X}"
                );
            }
        }
    }

    /// What Jellyfin 12.4 serves for bytes with no character in their charset
    /// (gl-2010): sidecars it named `cp949`, `gb18030`, `big5`, `shift-jis`
    /// and `euc-jp`, each sequence before text that shows how many bytes the
    /// decoder took.
    #[rstest::rstest]
    // Code page 949: `?` for a lead byte and the byte after it, whatever it is.
    #[case::cp949_unassigned(encoding_rs::EUC_KR, b"\xa2\xe8a", "?a")]
    #[case::cp949_no_trail_byte(encoding_rs::EUC_KR, b"\x81\x5ba", "?a")]
    #[case::cp949_ascii(encoding_rs::EUC_KR, b"\xa1\x30a", "?a")]
    #[case::cp949_line_feed(encoding_rs::EUC_KR, b"\x81\na", "?a")]
    #[case::cp949_user_defined_lead(encoding_rs::EUC_KR, b"\xc9\xa0a", "?a")]
    #[case::cp949_last_lead_byte(encoding_rs::EUC_KR, b"\xfe a", "?a")]
    #[case::cp949_lone_bytes(encoding_rs::EUC_KR, b"\x80a\xffa", "\u{80}a\u{F8F7}a")]
    #[case::cp949_end(encoding_rs::EUC_KR, b"\xc7\xd1\xc9", "한?")]
    #[case::cp949_no_text(encoding_rs::EUC_KR, b"", "")]
    // GB18030: `?` for the first byte alone, and for a four-byte sequence
    // that is well formed and has no character.
    #[case::gb18030_four_bytes(encoding_rs::GBK, b"\x84\x31\xa5\x30a", "?a")]
    #[case::gb18030_beyond_unicode(encoding_rs::GBK, b"\xe3\x32\x9a\x36a", "?a")]
    #[case::gb18030_lead_before_ff(encoding_rs::GBK, b"\x81\xffa", "??a")]
    #[case::gb18030_lead_before_ascii(encoding_rs::GBK, b"\x81 a", "? a")]
    #[case::gb18030_broken_four_bytes(encoding_rs::GBK, b"\x81\x30\x81\x41", "?0\u{4E04}")]
    #[case::gb18030_lone_ff(encoding_rs::GBK, b"\xffa", "?a")]
    #[case::gb18030_end(encoding_rs::GBK, b"\x81\x30\x81", "?0?")]
    // Code page 950: as 949, with its own lone bytes.
    #[case::big5_unassigned(encoding_rs::BIG5, b"\xa3\xe2a", "?a")]
    #[case::big5_last_unassigned(encoding_rs::BIG5, b"\xa3\xfea", "?a")]
    #[case::big5_no_trail_byte(encoding_rs::BIG5, b"\x81 a\xfe a", "?a?a")]
    #[case::big5_beside_the_trail_bytes(
        encoding_rs::BIG5,
        b"\x81\x3fa\x81\x7fa\x81\xa0a\x81\xffa",
        "?a?a?a?a"
    )]
    #[case::big5_line_feed(encoding_rs::BIG5, b"\xa1\na", "?a")]
    #[case::big5_lone_bytes(encoding_rs::BIG5, b"\x80a\xffa", "\u{80}a\u{F8F8}a")]
    #[case::big5_end(encoding_rs::BIG5, b"\xa4\xa4\xa4", "中?")]
    // Code page 932: its default character U+30FB where the others write `?`.
    #[case::shift_jis_unassigned(encoding_rs::SHIFT_JIS, b"\x81\xada", "\u{30FB}a")]
    #[case::shift_jis_ascii_trail(encoding_rs::SHIFT_JIS, b"\x82\x40a", "\u{30FB}a")]
    #[case::shift_jis_no_trail_byte(encoding_rs::SHIFT_JIS, b"\x81\x30a", "\u{30FB}a")]
    #[case::shift_jis_line_feed(encoding_rs::SHIFT_JIS, b"\xe0\na", "\u{30FB}a")]
    #[case::shift_jis_lead_byte_ranges(
        encoding_rs::SHIFT_JIS,
        b"\x9f a\xe0 a\xfc a",
        "\u{30FB}a\u{30FB}a\u{30FB}a"
    )]
    #[case::shift_jis_lone_bytes(
        encoding_rs::SHIFT_JIS,
        b"\xa0a\xfd\xfe\xffa",
        "\u{F8F0}a\u{F8F1}\u{F8F2}\u{F8F3}a"
    )]
    #[case::shift_jis_end(encoding_rs::SHIFT_JIS, b"\x82\xa0\x81", "あ\u{30FB}")]
    // Code page 51932 reads 932's table: 8E and A1 to FE are lead bytes, 8F
    // is not, and a byte below A0 that is no lead byte is U+30FB alone.
    #[case::euc_jp_unassigned(encoding_rs::EUC_JP, b"\xa3\xa1a", "\u{30FB}a")]
    #[case::euc_jp_no_katakana(encoding_rs::EUC_JP, b"\x8e\xe0a", "\u{30FB}a")]
    #[case::euc_jp_line_feed(encoding_rs::EUC_JP, b"\xa1\na", "\u{30FB}a")]
    #[case::euc_jp_8f_alone(encoding_rs::EUC_JP, b"\x8f\xa1\xa1a", "\u{30FB}\u{3000}a")]
    #[case::euc_jp_8f_end(encoding_rs::EUC_JP, b"\x8f\xa1", "\u{30FB}\u{30FB}")]
    #[case::euc_jp_lead_byte_ranges(
        encoding_rs::EUC_JP,
        b"\x8e a\xa1 a\xfe a",
        "\u{30FB}a\u{30FB}a\u{30FB}a"
    )]
    #[case::euc_jp_no_lead_byte(
        encoding_rs::EUC_JP,
        b"\x85a\x8d a\x8f a\x9f a",
        "\u{30FB}a\u{30FB} a\u{30FB} a\u{30FB} a"
    )]
    #[case::euc_jp_lone_bytes(encoding_rs::EUC_JP, b"\x80a\xa0a\xffa", "\u{80}a\u{F8F0}a\u{F8F3}a")]
    #[case::euc_jp_end(encoding_rs::EUC_JP, b"\xa4\xa2\xa1", "あ\u{30FB}")]
    fn bytes_with_no_character_are_read_as_dotnet_reads_them(
        #[case] encoding: &'static encoding_rs::Encoding,
        #[case] bytes: &[u8],
        #[case] expected: &str,
    ) {
        assert_eq!(decode(encoding, bytes), expected);
    }

    /// The bytes .NET's single-byte code pages have no character for are
    /// Private Use Area code points, read from Jellyfin 12.4 through a
    /// `ComicInfo.xml` declaring each encoding: the first and the last byte of
    /// every run.
    #[rstest::rstest]
    #[case(encoding_rs::WINDOWS_1253, b"\xaa\xd2\xff", "\u{F8F9}\u{F8FA}\u{F8FB}")]
    #[case(
        encoding_rs::WINDOWS_1255,
        b"\xd9\xdf\xfb\xfc\xff",
        "\u{F88D}\u{F893}\u{F894}\u{F895}\u{F896}"
    )]
    #[case(encoding_rs::WINDOWS_1257, b"\xa1\xa5", "\u{F8FC}\u{F8FD}")]
    #[case(
        encoding_rs::WINDOWS_874,
        b"\xdb\xde\xfc\xff",
        "\u{F8C1}\u{F8C4}\u{F8C5}\u{F8C8}"
    )]
    #[case(
        encoding_rs::ISO_8859_6,
        b"\xa1\xa3\xa5\xab\xae\xba\xbc\xbe\xc0\xdb\xdf\xf3\xff",
        "\u{F7C8}\u{F7CA}\u{F7CB}\u{F7D1}\u{F7D2}\u{F7DE}\u{F7DF}\u{F7E1}\u{F7E2}\u{F7E3}\u{F7E7}\u{F7E8}\u{F7F4}"
    )]
    #[case(encoding_rs::ISO_8859_7, b"\xae\xd2\xff", "\u{F7C5}\u{F7C6}\u{F7C7}")]
    #[case(
        encoding_rs::ISO_8859_8,
        b"\xa1\xbf\xde\xfb\xfc\xff",
        "\u{F79C}\u{F79D}\u{F7BC}\u{F7BD}\u{F7BE}\u{F7C1}"
    )]
    fn single_byte_code_pages_read_their_unassigned_bytes_as_dotnet_does(
        #[case] encoding: &'static encoding_rs::Encoding,
        #[case] bytes: &[u8],
        #[case] expected: &str,
    ) {
        assert_eq!(decode(encoding, bytes), expected);
    }

    /// What stays as it was, each an open question of its own: GB18030's lone
    /// 80 (gl-2008), a pair code page 950 has in its user-defined ranges
    /// (gl-2012), and JIS X 0212, which code page 51932 does not decode.
    #[test]
    fn differences_that_wait_for_a_decision_are_left_alone() {
        assert_eq!(decode(encoding_rs::GBK, b"\x80a"), "\u{20AC}a");
        assert_eq!(decode(encoding_rs::BIG5, b"\x81\x40a"), "\u{FFFD}@a");
        assert_eq!(decode(encoding_rs::BIG5, b"\x81\x7ea"), "\u{FFFD}~a");
        assert_eq!(decode(encoding_rs::BIG5, b"\x81\xa1a"), "\u{FFFD}a");
        assert_eq!(decode(encoding_rs::BIG5, b"\x81\xfea"), "\u{FFFD}a");
        assert_eq!(decode(encoding_rs::EUC_JP, b"\x8f\xb0\xa1a"), "\u{4E02}a");
    }

    /// The double-byte charsets [`decode`] reads as .NET does.
    fn double_byte() -> [&'static encoding_rs::Encoding; 5] {
        [
            encoding_rs::EUC_KR,
            encoding_rs::GBK,
            encoding_rs::BIG5,
            encoding_rs::SHIFT_JIS,
            encoding_rs::EUC_JP,
        ]
    }

    /// Every byte and every pair `encoding_rs` decodes is read as before:
    /// only what it cannot decode changes.
    #[test]
    fn what_encoding_rs_decodes_is_read_the_same() {
        for encoding in double_byte() {
            for first in 0x00..=0xFF_u8 {
                for second in 0x00..=0xFF_u8 {
                    let bytes = [first, second];
                    let Some(text) =
                        encoding.decode_without_bom_handling_and_without_replacement(&bytes)
                    else {
                        continue;
                    };
                    assert_eq!(
                        decode(encoding, &bytes),
                        text,
                        "{} {first:02X}{second:02X}",
                        encoding.name()
                    );
                }
            }
        }
    }

    /// No pair is left as U+FFFD but the 1,159 Big5 pairs code page 950 has
    /// in its user-defined ranges, which Jellyfin 12.4 serves as Private Use
    /// Area code points (gl-2012).
    #[test]
    fn only_code_page_950s_user_defined_pairs_stay_the_replacement_character() {
        for encoding in double_byte() {
            let mut left = 0;
            for first in 0x80..=0xFF_u8 {
                for second in 0x00..=0xFF_u8 {
                    if decode(encoding, &[first, second]).contains('\u{FFFD}') {
                        left += 1;
                    }
                }
            }
            let expected = if encoding == encoding_rs::BIG5 {
                1159
            } else {
                0
            };
            assert_eq!(left, expected, "{}", encoding.name());
        }
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
