//! What .NET's code pages read otherwise than `encoding_rs` (gl-2000, gl-2010,
//! gl-2011).
//!
//! Jellyfin decodes a legacy-encoded subtitle or playlist, and an XML
//! document that declares a legacy encoding, with .NET's code pages
//! (`System.Text.Encoding.CodePages`), and Ferrofin with `encoding_rs`, whose
//! tables and error handling are the WHATWG Encoding Standard's. [`decode`],
//! and [`Decoder`] for a document that names its encoding, read three things
//! as .NET does:
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
//! pair of each charset, and books and XMLTV guides whose XML declares each
//! encoding, served by a stock server and by Ferrofin, and the text compared
//! pair by pair. The `escape-charsets` parity row lists what else that
//! comparison found.
//!
//! Inside this module [`encoding_rs::GB18030`] stands for .NET's
//! `GB18030Encoding` and [`encoding_rs::GBK`] for its code page 936, which
//! replace what they cannot decode differently. A charset detector's GBK is
//! the former ([`decode`]); a document names one or the other
//! ([`Decoder::declared`]).

use encoding_rs::{CoderResult, DecoderResult, Encoding};

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
/// named `cp949`, `big5`, `shift-jis` and `euc-jp`, and for code page 936 and
/// the single-byte code pages from a `ComicInfo.xml` declaring each one
/// (gl-2011 for 936, ISO-8859-3 and ISO-8859-8-I). Code page 51932 (`euc-jp`)
/// reads 932's table. All but the three U+0080 are Private Use Area code
/// points.
///
/// Empty for an encoding [`decode`] leaves to `encoding_rs`.
fn lone_bytes(encoding: &'static Encoding) -> &'static [LoneBytes] {
    if encoding == encoding_rs::EUC_KR {
        &[(0x80, 0x80, 0x80), (0xFF, 0xFF, 0xF8F7)]
    } else if encoding == encoding_rs::GBK {
        &[(0xFF, 0xFF, 0xF8F5)]
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
    } else if encoding == encoding_rs::ISO_8859_3 {
        &[
            (0xA5, 0xA5, 0xF7F5),
            (0xAE, 0xAE, 0xF7F6),
            (0xBE, 0xBE, 0xF7F7),
            (0xC3, 0xC3, 0xF7F8),
            (0xD0, 0xD0, 0xF7F9),
            (0xE3, 0xE3, 0xF7FA),
            (0xF0, 0xF0, 0xF7FB),
        ]
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
    } else if encoding == encoding_rs::ISO_8859_8 || encoding == encoding_rs::ISO_8859_8_I {
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
    if encoding == encoding_rs::EUC_KR
        || encoding == encoding_rs::BIG5
        || encoding == encoding_rs::GBK
    {
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
/// Measured on Jellyfin 12.4 (gl-2010, and gl-2011 for code page 936), every
/// high byte before every other byte and at the end of the text, and as .NET
/// 10's `GetChars` reads:
/// - `GB18030Encoding` writes `?` for the first byte alone, and for the four
///   bytes of a well-formed sequence that has no character;
/// - `DBCSCodePageEncoding` (code pages 949, 936, 950, 932 and 51932) writes
///   one character for a lead byte and the byte after it, whatever that byte
///   is (a line feed too), and for a lead byte the text ends in. Code page 936
///   has no four-byte form, so one that has no character is two such pairs;
/// - a byte that is no lead byte is what [`lone_bytes`] holds.
///
/// The character is `?`, and code page 932's default character U+30FB for
/// `shift-jis` and `euc-jp`.
fn dotnet_fallback(encoding: &'static Encoding, tail: &[u8]) -> Option<(char, usize)> {
    let (&first, rest) = tail.split_first()?;
    if encoding == encoding_rs::GB18030 {
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

/// Whether `encoding_rs` reads `encoding` as its .NET code page does, bytes
/// with no character included, so that nothing here applies to it.
fn is_left_to_encoding_rs(encoding: &'static Encoding) -> bool {
    encoding != encoding_rs::GB18030 && lone_bytes(encoding).is_empty()
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
/// `encoding` is what a charset detector named: [`encoding_rs::GBK`] is read
/// with .NET's `GB18030Encoding`, the only name `UTF.Unknown` has for 8-bit
/// Simplified Chinese. A byte order mark is not looked for in those charsets:
/// callers rule one out before they detect a charset.
#[must_use]
pub fn decode(encoding: &'static Encoding, bytes: &[u8]) -> String {
    let encoding = if encoding == encoding_rs::GBK {
        encoding_rs::GB18030
    } else {
        encoding
    };
    if is_left_to_encoding_rs(encoding) {
        return encoding.decode(bytes).0.into_owned();
    }
    let mut out = String::new();
    Decoder::declared(encoding).decode(bytes, true, &mut out);
    out
}

/// A legacy-encoded text decoded as it arrives, piece by piece, to the text
/// [`decode`] gives it whole: for an XML document, which names its encoding
/// and may be too large to hold.
pub struct Decoder {
    /// The encoding, as this module reads [`encoding_rs::GBK`] and
    /// [`encoding_rs::GB18030`].
    encoding: &'static Encoding,
    /// `encoding_rs`'s own decoder, for an encoding it reads as .NET does: it
    /// keeps what one piece leaves for the next, ISO-2022-JP's state too.
    /// `None` for the encodings [`decode_some`] reads.
    whatwg: Option<encoding_rs::Decoder>,
    /// The end of the last piece, when .NET's decoder would look past it:
    /// three bytes at most.
    held: Vec<u8>,
}

impl Decoder {
    /// The decoder for the encoding a document's declaration names, where
    /// .NET's `Encoding.GetEncoding` tells two code pages apart that a
    /// detector cannot: [`encoding_rs::GBK`] (`gb2312`, `gbk`) is code page
    /// 936 here, which takes a lead byte and the byte after it for one `?`
    /// and has no four-byte form, and only [`encoding_rs::GB18030`] is
    /// `GB18030Encoding`.
    ///
    /// A byte with no character is never an error: the text holds what
    /// .NET's decoder writes for it, and U+FFFD in an encoding this module
    /// leaves to `encoding_rs`.
    ///
    /// Read as before, and decided elsewhere:
    /// - [`encoding_rs::EUC_KR`] is code page 949 whichever name the document
    ///   uses; .NET reads `euc-kr` and `cseuckr` with code page 51949
    ///   (gl-2008, questions B and C);
    /// - a four-byte GB18030 sequence that has a character keeps it in a
    ///   document naming code page 936, where .NET writes `??` (gl-2008,
    ///   question E), and 81 35 F4 37 stays U+E7C7 there;
    /// - a pair or a byte `encoding_rs` decodes to another character than the
    ///   .NET code page (gl-2008, question A, and gl-2012 for Big5).
    #[must_use]
    pub fn declared(encoding: &'static Encoding) -> Self {
        Self {
            encoding,
            whatwg: is_left_to_encoding_rs(encoding)
                .then(|| encoding.new_decoder_without_bom_handling()),
            held: Vec::new(),
        }
    }

    /// Appends the text of `bytes`, the next piece of the document, to `out`.
    /// `last` says no piece follows, so a sequence the bytes end in is read
    /// as it stands.
    pub fn decode(&mut self, bytes: &[u8], last: bool, out: &mut String) {
        if let Some(decoder) = self.whatwg.as_mut() {
            let mut rest = bytes;
            loop {
                out.reserve(
                    decoder
                        .max_utf8_buffer_length(rest.len())
                        .unwrap_or(rest.len()),
                );
                let (result, read, _) = decoder.decode_to_string(rest, out, last);
                rest = rest.get(read..).unwrap_or_default();
                if result == CoderResult::InputEmpty {
                    return;
                }
            }
        }
        let from = out.len();
        let joined;
        let window = if self.held.is_empty() {
            bytes
        } else {
            joined = [self.held.as_slice(), bytes].concat();
            joined.as_slice()
        };
        let read = decode_some(self.encoding, window, last, out);
        self.held = window.get(read..).unwrap_or_default().to_vec();
        if self.encoding == encoding_rs::GB18030
            && let Some(decoded) = out.get(from..)
            && decoded.contains('\u{E7C7}')
        {
            let decoded = decoded.replace('\u{E7C7}', "\u{1E3F}");
            out.truncate(from);
            out.push_str(&decoded);
        }
    }
}

/// How far past the first byte of a sequence [`dotnet_fallback`] looks: the
/// four bytes of a GB18030 sequence.
const FALLBACK_BYTES: usize = 4;

/// Appends to `out` the text of `bytes` in `encoding`, an encoding
/// [`lone_bytes`] or [`dotnet_fallback`] knows, and returns how many bytes it
/// read. With `last` false it stops before a sequence that the next bytes may
/// complete, or that [`dotnet_fallback`] would look past the end of `bytes`
/// to replace: fewer than [`FALLBACK_BYTES`] are then left unread.
///
/// None of these encodings keeps state between characters, so the bytes left
/// unread start a character and a later call can begin at them.
fn decode_some(encoding: &'static Encoding, bytes: &[u8], last: bool, out: &mut String) -> usize {
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut position = 0;
    loop {
        let rest = bytes.get(position..).unwrap_or_default();
        out.reserve(
            decoder
                .max_utf8_buffer_length_without_replacement(rest.len())
                .unwrap_or(rest.len()),
        );
        // Flushed whatever `last` says: a sequence the bytes end in then
        // comes back as malformed, which tells where it starts.
        let (result, read) = decoder.decode_to_string_without_replacement(rest, out, true);
        position += read;
        match result {
            DecoderResult::InputEmpty => return position,
            DecoderResult::OutputFull => {}
            DecoderResult::Malformed(length, after) => {
                // The sequence ends `after` bytes before the last byte read.
                let end = position.saturating_sub(usize::from(after));
                let start = end.saturating_sub(usize::from(length));
                if !last && bytes.len() - start < FALLBACK_BYTES {
                    return start;
                }
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
}

#[cfg(test)]
mod tests {
    use super::{Decoder, decode, user_defined};

    /// `bytes` as a document naming `encoding` is read, in pieces of `step`
    /// bytes.
    fn declared(encoding: &'static encoding_rs::Encoding, bytes: &[u8], step: usize) -> String {
        let mut decoder = Decoder::declared(encoding);
        let mut out = String::new();
        let mut pieces = bytes.chunks(step).peekable();
        while let Some(piece) = pieces.next() {
            decoder.decode(piece, pieces.peek().is_none(), &mut out);
        }
        out
    }

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
    #[case(
        encoding_rs::ISO_8859_8_I,
        b"\xa1\xbf\xde\xfb\xfc\xff",
        "\u{F79C}\u{F79D}\u{F7BC}\u{F7BD}\u{F7BE}\u{F7C1}"
    )]
    #[case(
        encoding_rs::ISO_8859_3,
        b"\xa5\xae\xbe\xc3\xd0\xe3\xf0",
        "\u{F7F5}\u{F7F6}\u{F7F7}\u{F7F8}\u{F7F9}\u{F7FA}\u{F7FB}"
    )]
    fn single_byte_code_pages_read_their_unassigned_bytes_as_dotnet_does(
        #[case] encoding: &'static encoding_rs::Encoding,
        #[case] bytes: &[u8],
        #[case] expected: &str,
    ) {
        assert_eq!(decode(encoding, bytes), expected);
    }

    /// Code page 936, which a document names `gb2312` or `gbk` (gl-2011), as
    /// Jellyfin 12.4 reads a `ComicInfo.xml` and an XMLTV guide declaring it:
    /// one `?` for a lead byte and the byte after it, as the other double-byte
    /// code pages write, where `gb18030` writes it for the lead byte alone;
    /// no four-byte form; 80 and FF read alone.
    #[rstest::rstest]
    #[case::lead_before_ascii(b"\x81 a", "?a")]
    #[case::lead_before_ff(b"\x81\xffa", "?a")]
    #[case::lead_before_a_digit(b"\x81\x30a", "?a")]
    #[case::line_feed(b"\x81\na", "?a")]
    #[case::four_bytes_with_no_character(b"\x84\x31\xa5\x30a", "??a")]
    #[case::broken_four_bytes(b"\x81\x30\x81\x41", "?\u{4E04}")]
    #[case::lone_bytes(b"\x80a\xffa", "\u{20AC}a\u{F8F5}a")]
    #[case::lone_80_before_a_lead_byte(b"\x80\x81 a", "\u{20AC}?a")]
    #[case::end(b"\xd6\xd0\x81", "中?")]
    fn a_document_naming_code_page_936_is_read_as_dotnet_reads_it(
        #[case] bytes: &[u8],
        #[case] expected: &str,
        #[values(1, 2, 3, usize::MAX)] step: usize,
    ) {
        assert_eq!(declared(encoding_rs::GBK, bytes, step), expected);
    }

    /// A document naming `gb18030` is read as a detector's GBK is: every byte
    /// pair, before a digit pair that may make a four-byte sequence of it.
    #[test]
    fn a_document_naming_gb18030_is_read_as_detected_chinese_text_is() {
        for first in 0x80..=0xFF_u8 {
            for second in 0x00..=0xFF_u8 {
                let bytes = [first, second, 0x81, b'0', b' '];
                assert_eq!(
                    declared(encoding_rs::GB18030, &bytes, usize::MAX),
                    decode(encoding_rs::GBK, &bytes),
                    "{first:02X}{second:02X}"
                );
            }
        }
        assert_eq!(
            declared(encoding_rs::GB18030, b"\x81\x35\xf4\x37", usize::MAX),
            "\u{1E3F}"
        );
    }

    /// Text read in pieces is the text read whole, wherever the pieces end:
    /// every byte pair of each charset before a digit pair, so that a
    /// character, a replaced sequence and a four-byte form are all cut.
    #[test]
    fn text_read_in_pieces_is_the_text_read_whole() {
        let charsets = [
            encoding_rs::EUC_KR,
            encoding_rs::GBK,
            encoding_rs::GB18030,
            encoding_rs::BIG5,
            encoding_rs::SHIFT_JIS,
            encoding_rs::EUC_JP,
            encoding_rs::WINDOWS_1255,
            encoding_rs::WINDOWS_1252,
        ];
        for encoding in charsets {
            for first in 0x80..=0xFF_u8 {
                for second in 0x00..=0xFF_u8 {
                    let bytes = [first, second, 0x81, b'0', b' ', first];
                    let whole = declared(encoding, &bytes, usize::MAX);
                    for step in 1..=3 {
                        assert_eq!(
                            declared(encoding, &bytes, step),
                            whole,
                            "{} {first:02X}{second:02X} by {step}",
                            encoding.name()
                        );
                    }
                }
            }
        }
    }

    /// An encoding left to `encoding_rs` keeps its decoder from one piece to
    /// the next, ISO-2022-JP's state with it, and writes U+FFFD for what it
    /// cannot decode.
    #[rstest::rstest]
    #[case(encoding_rs::ISO_2022_JP, b"\x1b$B0!0!\x1b(Ba\xe9", "亜亜a\u{FFFD}")]
    #[case(encoding_rs::WINDOWS_1252, b"caf\xe9 \x81", "café \u{81}")]
    #[case(encoding_rs::GB18030, b"\x81\x35\xf4\x37", "\u{1E3F}")]
    fn a_decoder_keeps_what_one_piece_leaves_for_the_next(
        #[case] encoding: &'static encoding_rs::Encoding,
        #[case] bytes: &[u8],
        #[case] expected: &str,
        #[values(1, 2, 3, usize::MAX)] step: usize,
    ) {
        assert_eq!(declared(encoding, bytes, step), expected);
    }

    /// What stays as it was in a document naming code page 936, each an open
    /// question (gl-2008): a pair 936 has in the Private Use Area and
    /// `encoding_rs` decodes to a character, and GB18030's four-byte form,
    /// which 936 does not have (81 35 F4 37 keeps `encoding_rs`'s code point
    /// there).
    #[test]
    fn code_page_936_differences_that_wait_for_a_decision_are_left_alone() {
        let read = |bytes: &[u8]| declared(encoding_rs::GBK, bytes, usize::MAX);
        assert_eq!(read(b"\xa2\xe3a"), "\u{20AC}a");
        assert_eq!(read(b"\xfe\x50a"), "\u{2E81}a");
        assert_eq!(read(b"\x81\x30\x81\x30a"), "\u{80}a");
        assert_eq!(read(b"\x81\x35\xf4\x37a"), "\u{E7C7}a");
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
