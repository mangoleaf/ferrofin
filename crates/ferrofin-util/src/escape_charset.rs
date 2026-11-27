//! The 7-bit escape encodings Jellyfin detects and decodes beside ISO-2022-JP:
//! ISO-2022-KR and HZ-GB-2312 (gl-1657).
//!
//! Port of the escape detection in UTF.Unknown 2.7.0, the version Jellyfin
//! 12.4 pins:
//! - `CharsetDetector.Feed` and `FindInputState` decide which bytes reach the
//!   escape prober;
//! - `EscCharsetProber` runs the `HZ_GB_2312_SMModel`, `Iso_2022_JP_SMModel`
//!   and `Iso_2022_KR_SMModel` state machines over them. Their tables are
//!   copied verbatim below.
//!
//! The first machine to reach `ITSME` names the charset. The prober's
//! `Iso_2022_CN_SMModel` is left out: in 2.7.0 its byte classes give `$`
//! class 0, which the state after `ESC` errors on, so it never names
//! ISO-2022-CN. Jellyfin decodes the two charsets here through .NET's code
//! pages 50225 and 52936. ISO-2022-JP's callers decode through `chardetng`
//! (gl-1422), so its machine only decides which designation comes first.

/// A 7-bit escape encoding [`detect`] finds and [`EscapeCharset::decode`]
/// decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeCharset {
    /// HZ-GB-2312 (RFC 1843): GB 2312 between `~{` and `~}`, with the high
    /// bit of each byte cleared.
    HzGb2312,
    /// ISO-2022-KR (RFC 1557): KS X 1001 between `SO` and `SI` after the
    /// designation `ESC $ ) C`, with the high bit of each byte cleared.
    Iso2022Kr,
}

impl EscapeCharset {
    /// UTF.Unknown's name for the charset (`CodepageName`), which Jellyfin
    /// passes ffmpeg as a subtitle's character encoding.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::HzGb2312 => "hz-gb-2312",
            Self::Iso2022Kr => "iso-2022-kr",
        }
    }

    /// `bytes` decoded to text as .NET decodes them for Jellyfin: ports of
    /// `ISO2022Encoding.GetCharsCP52936` and `GetCharsCP50225KR` (.NET 10).
    /// Each pair is looked up in the 8-bit table it carries (code page 936,
    /// which `encoding_rs`'s GBK reads, and 949's KS X 1001 rows, EUC-KR), and
    /// what fails is `?`, the replacement those decoders fall back to.
    #[must_use]
    pub fn decode(self, bytes: &[u8]) -> String {
        match self {
            Self::HzGb2312 => decode_hz(bytes),
            Self::Iso2022Kr => decode_iso_2022_kr(bytes),
        }
    }
}

/// `CharsetDetector.BufferSize`: the input is read, and its state found, in
/// chunks of this many bytes.
const BUFFER_SIZE: usize = 1024;

/// `StateMachineModel.START`.
const START: u8 = 0;
/// `StateMachineModel.ERROR`.
const ERROR: u8 = 1;
/// `StateMachineModel.ITSME`.
const ITS_ME: u8 = 2;

/// One of the escape prober's machines: a `StateMachineModel` with its byte
/// classes, class factor and state table, and the charset it names.
struct Model {
    /// The charset it names, `None` for ISO-2022-JP.
    charset: Option<EscapeCharset>,
    /// The byte's class (the model's `cls` table).
    class: fn(u8) -> usize,
    /// The number of classes, which strides the state table.
    class_factor: usize,
    /// The next state, at `state * class_factor + class`.
    states: &'static [u8],
}

/// `HZ_GB_2312_SMModel`'s classes: NUL, `ESC` and every high byte 1, `~` 2,
/// `{` 4, `}` 5, the rest 0.
fn hz_class(byte: u8) -> usize {
    match byte {
        0x00 | 0x1B | 0x80..=0xFF => 1,
        b'~' => 2,
        b'{' => 4,
        b'}' => 5,
        _ => 0,
    }
}

/// `HZ_GB_2312_SMModel.HZ_st`.
const HZ_STATES: [u8; 48] = [
    START, ERROR, 3, START, START, START, ERROR, ERROR, //
    ERROR, ERROR, ERROR, ERROR, ITS_ME, ITS_ME, ITS_ME, ITS_ME, //
    ITS_ME, ITS_ME, ERROR, ERROR, START, START, 4, ERROR, //
    5, ERROR, 6, ERROR, 5, 5, 4, ERROR, //
    4, ERROR, 4, 4, 4, ERROR, 4, ERROR, //
    4, ITS_ME, START, START, START, START, START, START,
];

/// `Iso_2022_JP_SMModel`'s classes: `ESC` 1; NUL, `SO`, `SI` and every high
/// byte 2; `(` 3, `B` 4, `J` 5, `@` 6, `$` 7, `D` 8, `I` 9; the rest 0.
fn jp_class(byte: u8) -> usize {
    match byte {
        0x1B => 1,
        0x00 | 0x0E | 0x0F | 0x80..=0xFF => 2,
        b'(' => 3,
        b'B' => 4,
        b'J' => 5,
        b'@' => 6,
        b'$' => 7,
        b'D' => 8,
        b'I' => 9,
        _ => 0,
    }
}

/// `Iso_2022_JP_SMModel.ISO2022JP_st`.
const JP_STATES: [u8; 72] = [
    START, 3, ERROR, START, START, START, START, START, //
    START, START, ERROR, ERROR, ERROR, ERROR, ERROR, ERROR, //
    ERROR, ERROR, ERROR, ERROR, ITS_ME, ITS_ME, ITS_ME, ITS_ME, //
    ITS_ME, ITS_ME, ITS_ME, ITS_ME, ITS_ME, ITS_ME, ERROR, ERROR, //
    ERROR, 5, ERROR, ERROR, ERROR, 4, ERROR, ERROR, //
    ERROR, ERROR, ERROR, 6, ITS_ME, ERROR, ITS_ME, ERROR, //
    ERROR, ERROR, ERROR, ERROR, ERROR, ERROR, ITS_ME, ITS_ME, //
    ERROR, ERROR, ERROR, ITS_ME, ERROR, ERROR, ERROR, ERROR, //
    ERROR, ERROR, ERROR, ERROR, ITS_ME, ERROR, START, START,
];

/// `Iso_2022_KR_SMModel`'s classes: `ESC` 1, NUL and every high byte 2, `$`
/// 3, `)` 4, `C` 5, the rest 0.
fn kr_class(byte: u8) -> usize {
    match byte {
        0x1B => 1,
        0x00 | 0x80..=0xFF => 2,
        b'$' => 3,
        b')' => 4,
        b'C' => 5,
        _ => 0,
    }
}

/// `Iso_2022_KR_SMModel.ISO2022KR_st`.
const KR_STATES: [u8; 40] = [
    START, 3, ERROR, START, START, START, ERROR, ERROR, //
    ERROR, ERROR, ERROR, ERROR, ITS_ME, ITS_ME, ITS_ME, ITS_ME, //
    ITS_ME, ITS_ME, ERROR, ERROR, ERROR, 4, ERROR, ERROR, //
    ERROR, ERROR, ERROR, ERROR, 5, ERROR, ERROR, ERROR, //
    ERROR, ERROR, ERROR, ITS_ME, START, START, START, START,
];

/// The machines, in `EscCharsetProber`'s order. No two reach `ITSME` on the
/// same byte, so the order never decides.
const MODELS: [Model; 3] = [
    Model {
        charset: Some(EscapeCharset::HzGb2312),
        class: hz_class,
        class_factor: 6,
        states: &HZ_STATES,
    },
    Model {
        charset: None,
        class: jp_class,
        class_factor: 10,
        states: &JP_STATES,
    },
    Model {
        charset: Some(EscapeCharset::Iso2022Kr),
        class: kr_class,
        class_factor: 6,
        states: &KR_STATES,
    },
];

/// The escape charset of `bytes`, found as `CharsetDetector` finds it in a
/// file.
///
/// The bytes are read in 1024-byte chunks, and each chunk's input state is
/// found first:
/// - a byte with its high bit set (but 0xA0) makes the input 8-bit, which
///   drops the escape prober for good;
/// - an `ESC`, or a `{` after a `~`, in 7-bit input starts escape detection.
///
/// From the chunk that starts it on, the prober steps every machine over each
/// byte. A machine that errors drops out, and the first to reach `ITSME`
/// names the charset.
///
/// `None` is returned when:
/// - the input is 8-bit, or nothing starts escape detection;
/// - every machine errors, or none reaches `ITSME`;
/// - ISO-2022-JP's designation comes first. Callers detect that charset
///   through `chardetng`.
#[must_use]
pub fn detect(bytes: &[u8]) -> Option<EscapeCharset> {
    let mut escaped = false;
    let mut last = 0;
    let mut active: Vec<(&Model, u8)> = MODELS.iter().map(|model| (model, START)).collect();
    for chunk in bytes.chunks(BUFFER_SIZE) {
        for &byte in chunk {
            if byte & 0x80 != 0 && byte != 0xA0 {
                return None;
            }
            escaped |= byte == 0x1B || (byte == b'{' && last == b'~');
            last = byte;
        }
        if !escaped {
            continue;
        }
        for &byte in chunk {
            let mut index = 0;
            while let Some((model, state)) = active.get_mut(index) {
                *state =
                    model.states[usize::from(*state) * model.class_factor + (model.class)(byte)];
                match *state {
                    ERROR => {
                        active.swap_remove(index);
                        if active.is_empty() {
                            return None;
                        }
                    }
                    ITS_ME => return model.charset,
                    _ => index += 1,
                }
            }
        }
    }
    None
}

/// Pushes what `encoding`'s 8-bit pair `lead`, `trail` decodes to, or `?`.
fn push_pair(out: &mut String, encoding: &'static encoding_rs::Encoding, lead: u8, trail: u8) {
    match encoding.decode_without_bom_handling_and_without_replacement(&[lead, trail]) {
        Some(text) => out.push_str(&text),
        None => out.push('?'),
    }
}

/// Port of `GetCharsCP50225KR`:
/// - an `ESC` and the three bytes after it are held back and read as the
///   designation `ESC $ ) C`, which is dropped wherever it stands; anything
///   else held back is read again byte by byte (`CheckEscapeSequenceKR`,
///   `DecrementEscapeBytes`);
/// - `SO` starts a run of KS X 1001 and `SI` ends it;
/// - in a run, every byte but a space, tab or line feed leads a pair with the
///   byte after it, `SI` included, unless that byte is a held-back `ESC`, and
///   the pair is looked up in [`push_kr`]'s table;
/// - outside a run a byte is looked up alone.
///
/// A lead byte with nothing after it is `?`.
fn decode_iso_2022_kr(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut korean = false;
    let mut held: std::collections::VecDeque<u8> = std::collections::VecDeque::with_capacity(4);
    let mut input = bytes.iter().copied();
    loop {
        let byte = if let Some(&first) = held.front() {
            if first == 0x1B
                && let Some(next) = input.next()
            {
                held.push_back(next);
                if held.len() < 4 {
                    continue;
                }
                if held.iter().copied().eq(*b"\x1b$)C") {
                    held.clear();
                    continue;
                }
            }
            match held.pop_front() {
                Some(byte) => byte,
                None => break,
            }
        } else {
            let Some(byte) = input.next() else { break };
            if byte == 0x1B {
                held.push_back(byte);
                continue;
            }
            byte
        };
        match byte {
            0x0E => korean = true,
            0x0F => korean = false,
            _ if korean && !matches!(byte, b' ' | b'\t' | b'\n') => {
                let code = match held.front() {
                    Some(&0x1B) => u16::from(byte),
                    Some(_) => match held.pop_front() {
                        Some(trail) => u16::from_be_bytes([byte, trail]),
                        None => u16::from(byte),
                    },
                    None => {
                        let Some(trail) = input.next() else {
                            out.push('?');
                            break;
                        };
                        u16::from_be_bytes([byte, trail])
                    }
                };
                push_kr(&mut out, code);
            }
            _ => push_kr(&mut out, u16::from(byte)),
        }
    }
    out
}

/// Pushes what code page 50225's table holds for `code`, a byte or a pair
/// (lead byte high), or `?`. The table is 949's: its ASCII bytes, and its
/// KS X 1001 pairs (both bytes from 0xA1 to 0xFE) with the high bits
/// cleared. A pair with a NUL lead is its trail byte alone.
fn push_kr(out: &mut String, code: u16) {
    let [lead, trail] = code.to_be_bytes();
    match code {
        0x0000..=0x007F => out.push(char::from(trail)),
        0x0100..=0xFFFF if (b'!'..=b'~').contains(&lead) && (b'!'..=b'~').contains(&trail) => {
            push_pair(out, encoding_rs::EUC_KR, lead | 0x80, trail | 0x80);
        }
        _ => out.push('?'),
    }
}

/// Port of `GetCharsCP52936`:
/// - `~{` starts a run of GB 2312, `~}` ends one, and `~` with a line feed is
///   a line continuation, in either mode; `~~` is `~` outside a run, and any
///   other `~` is an ordinary byte;
/// - in a run, a byte below 0x20 is ASCII, and every other byte leads a pair
///   with the byte after it ([`push_hz_pair`]);
/// - outside a run a byte is ASCII, and 0x80 is `€`, as code page 936 reads
///   it.
///
/// Any other byte, a lead byte with no trail and a `~` that ends the input
/// are `?`.
fn decode_hz(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut gb = false;
    let mut rest = bytes;
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        if byte == b'~' {
            let Some((&next, tail)) = rest.split_first() else {
                out.push('?');
                break;
            };
            let switch = match next {
                b'~' if !gb => {
                    out.push('~');
                    true
                }
                b'{' => {
                    gb = true;
                    true
                }
                b'}' => {
                    gb = false;
                    true
                }
                b'\n' => true,
                _ => false,
            };
            if switch {
                rest = tail;
                continue;
            }
        }
        if gb && byte >= 0x20 {
            let Some((&trail, after)) = rest.split_first() else {
                out.push('?');
                break;
            };
            rest = after;
            push_hz_pair(&mut out, byte, trail);
        } else {
            match byte {
                0x00..=0x7F => out.push(char::from(byte)),
                0x80 => out.push('€'),
                _ => out.push('?'),
            }
        }
    }
    out
}

/// Pushes [`decode_hz`]'s reading of the pair `lead`, `trail` in a run:
/// - a lead from `!` to `w` and a trail from `!` to `~` are the 7-bit form of
///   a GB 2312 character, as is a pair with both high bits set;
/// - a space and any byte but NUL is that byte, and a lead from `!` to `}`
///   before a space is U+3000, quirks .NET keeps from its first decoder;
/// - anything else is `?`.
fn push_hz_pair(out: &mut String, lead: u8, trail: u8) {
    if lead == b' ' && trail != 0 {
        out.push(char::from(trail));
    } else if (b'!'..=b'w').contains(&lead) && (b'!'..=b'~').contains(&trail) {
        push_pair(out, encoding_rs::GBK, lead | 0x80, trail | 0x80);
    } else if (0xA1..=0xF7).contains(&lead) && (0xA1..=0xFE).contains(&trail) {
        push_pair(out, encoding_rs::GBK, lead, trail);
    } else if trail == b' ' && (b'!'..=b'}').contains(&lead) {
        out.push('\u{3000}');
    } else {
        out.push('?');
    }
}

#[cfg(test)]
mod tests {
    use super::{EscapeCharset, detect};

    /// `한국어 자막입니다.` as Python's `iso2022_kr` codec writes it.
    const KOREAN: &[u8] = b"\x1b$)C\x0eGQ19>n\x0f \x0e@Z87@T4O4Y\x0f.";
    /// `中文字幕。` as Python's `hz` codec writes it.
    const CHINESE: &[u8] = b"~{VPNDWVD;!#~}";

    #[rstest::rstest]
    #[case::korean(KOREAN, Some(EscapeCharset::Iso2022Kr))]
    #[case::chinese(CHINESE, Some(EscapeCharset::HzGb2312))]
    #[case::ascii(b"plain text", None)]
    // HZ's run must close at a pair boundary, and a `~` outside one must start
    // `~~` or `~{`: `~b` errors the machine.
    #[case::an_unclosed_run(b"~{VPND", None)]
    #[case::a_stray_tilde(b"a~b ~{VPND~}", None)]
    #[case::an_odd_run(b"~{VPN~}", None)]
    #[case::a_tilde_pair(b"a~~b ~{VPND~}", Some(EscapeCharset::HzGb2312))]
    // ISO-2022-JP's designation comes first: chardetng's to decode.
    #[case::japanese_first(b"\x1b$B$3\x1b(B \x1b$)C\x0eGQ\x0f", None)]
    #[case::korean_first(b"\x1b$)C\x0eGQ\x0f \x1b$B$3\x1b(B", Some(EscapeCharset::Iso2022Kr))]
    // ISO-2022-CN: every machine errors, so UTF.Unknown answers ASCII.
    #[case::iso_2022_cn(b"\x1b$)A\x0eVPND\x0f", None)]
    // A high byte makes the input 8-bit; 0xA0 does not, but errors the
    // machines it reaches.
    #[case::a_high_byte(b"\xe9 \x1b$)C\x0eGQ\x0f", None)]
    #[case::a_nbsp_before(b"\xa0\x1b$)C\x0eGQ\x0f", None)]
    #[case::a_nbsp_after(b"\x1b$)C\xa0\x0eGQ\x0f", Some(EscapeCharset::Iso2022Kr))]
    fn detects_as_utf_unknown_does(#[case] bytes: &[u8], #[case] expected: Option<EscapeCharset>) {
        assert_eq!(detect(bytes), expected, "{bytes:?}");
    }

    /// The input state is found per 1024-byte chunk before the chunk is
    /// probed. A chunk with no trigger is never probed, so a stray `~` there
    /// leaves HZ standing; in the trigger's own chunk it errors the machine. A
    /// high byte anywhere in a chunk drops the prober before the chunk is
    /// probed, but not after an earlier chunk named the charset.
    #[test]
    fn the_input_is_read_in_1024_byte_chunks() {
        let pad = |text: &[u8]| {
            let mut chunk = text.to_vec();
            chunk.resize(1024, b' ');
            chunk
        };
        let stray_then_hz = [pad(b"a~b"), CHINESE.to_vec()].concat();
        assert_eq!(detect(&stray_then_hz), Some(EscapeCharset::HzGb2312));
        let stray_with_hz = [b"a~b ".as_slice(), CHINESE].concat();
        assert_eq!(detect(&stray_with_hz), None);
        let korean_then_high = [pad(KOREAN), b"\xe9".to_vec()].concat();
        assert_eq!(detect(&korean_then_high), Some(EscapeCharset::Iso2022Kr));
        let korean_with_high = [KOREAN, b"\xe9"].concat();
        assert_eq!(detect(&korean_with_high), None);
    }

    #[rstest::rstest]
    #[case::subtitle(KOREAN, "한국어 자막입니다.")]
    #[case::ascii_around(b"\x1b$)C1 ~ \x0eGQ\x0f\n\x0e19>n\x0f", "1 ~ 한\n국어")]
    // RFC 1557 repeats the designation on every line.
    #[case::designation_per_line(b"\x1b$)C\x0eGQ\x0f\n\x1b$)C\x0e19>n\x0f", "한\n국어")]
    // Space, tab and line feed stay single in a run.
    #[case::white_space_in_a_run(b"\x1b$)C\x0eGQ\n19\t>n\x0f", "한\n국\t어")]
    // Any other byte leads a pair: an odd run swallows its `SI`, and a CR
    // pairs with the LF after it.
    #[case::an_odd_run(b"\x1b$)C\x0eGQ1\x0fab", "한?飡")]
    #[case::cr_in_a_run(b"\x1b$)C\x0eGQ\r\n\x0f", "한?")]
    #[case::a_lone_lead(b"\x1b$)C\x0eG", "?")]
    // An ESC that starts no designation is read again byte by byte; a lead
    // before a held-back ESC is looked up alone.
    #[case::another_escape(b"\x1b$)Ca\x1bb", "a\u{1b}b")]
    #[case::two_escapes_in_a_run(b"\x0e\x1b\x1b", "\u{1b}?")]
    #[case::an_escape_window_in_a_run(b"\x0eGQ\x1bab\x1b", "한?b?")]
    #[case::shift_out_in_an_escape_window(b"\x1b\x0e\xa0\x1b", "\u{1b}??")]
    // A replayed ESC starts a window of its own, and a lead takes an ESC that
    // is not held back yet as its trail.
    #[case::a_replayed_escape(b"\x1b\x1b$)Ca", "\u{1b}a")]
    #[case::an_escape_as_trail(b"\x0eG\x1b$)C", "?ㄹ?")]
    #[case::an_escape_trail_then_a_pair(b"\x0eG\x1bab", "?飡")]
    #[case::a_broken_then_a_whole_designation(b"\x1b$)\x1b$)Ca", "\u{1b}$)a")]
    // A NUL lead is its trail alone.
    #[case::a_nul_lead(b"\x0e\x00a\x00\x0f", "a\u{f}")]
    fn decodes_iso_2022_kr_as_dotnet_does(#[case] bytes: &[u8], #[case] text: &str) {
        assert_eq!(EscapeCharset::Iso2022Kr.decode(bytes), text, "{bytes:?}");
    }

    #[rstest::rstest]
    #[case::subtitle(CHINESE, "中文字幕。")]
    // `~~` is `~` outside a run, and `~` with a line feed continues the line,
    // inside a run too.
    #[case::tildes(b"a~~b ~{VPND~}~\nc\n~{WVD;~} ~x", "a~b 中文c\n字幕 ~x")]
    #[case::continued_run(b"~{VP~\nND~}", "中文")]
    // `~~` in a run is a pair of tildes, which GB 2312 lacks.
    #[case::tilde_pair_in_a_run(b"~{VP~~ND~}", "中?文")]
    // `~{` and `~}` switch in either mode; a control byte in a run is ASCII.
    #[case::redundant_switches(b"~}a~{VP~{ND\n~}", "a中文\n")]
    // .NET's first-decoder quirks: a space escapes the byte after it, and a
    // lead before a space is U+3000.
    #[case::space_escape(b"~{ AVP~}", "A中")]
    #[case::ideographic_space(b"~{V ~}", "\u{3000}")]
    #[case::bad_pair(b"~{xP~}", "?")]
    #[case::a_lone_lead(b"~{V", "?")]
    #[case::a_final_tilde(b"a~", "a?")]
    fn decodes_hz_gb_2312_as_dotnet_does(#[case] bytes: &[u8], #[case] text: &str) {
        assert_eq!(EscapeCharset::HzGb2312.decode(bytes), text, "{bytes:?}");
    }

    #[test]
    fn names_the_charsets_as_utf_unknown_does() {
        assert_eq!(EscapeCharset::Iso2022Kr.name(), "iso-2022-kr");
        assert_eq!(EscapeCharset::HzGb2312.name(), "hz-gb-2312");
    }
}
