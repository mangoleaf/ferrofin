//! Port of `StreamExtensions.cs` — read-all-lines plus byte-equality stream and
//! file comparison.
//!
//! The C# `MemoryStream`/`ArrayPool` fast paths are internal optimizations; only
//! the *observable contract* is reproduced here (per the port inventory):
//!
//! * Seekable streams are compared from the beginning (position reset to 0 on
//!   entry).
//! * Non-seekable streams are compared from their current read position.
//! * `is_stream_identical` does not restore positions.
//! * `is_file_identical` resets the stream to 0, compares, then restores the
//!   original position; it errors if the stream is not seekable.
//!
//! The port is synchronous (`std::io`); no ported test requires async, and
//! tokio is not in scope for this crate.

use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::StreamError;

/// I/O compare buffer size (bytes). Surfaced as config; matches the upstream
/// `StreamComparisonBufferSize = 81920`.
pub const STREAM_COMPARISON_BUFFER_SIZE: usize = 81920;

/// Reads all lines from a reader as UTF-8, dropping the line terminators.
///
/// # Errors
///
/// Returns any I/O error encountered while reading.
pub fn read_all_lines<R: Read>(reader: R) -> io::Result<Vec<String>> {
    BufReader::new(reader).lines().collect()
}

/// A byte source whose seekability is known, mirroring the C# `Stream.CanSeek`
/// branch that governs whether a comparison rewinds to the start.
pub trait SeekableRead: Read {
    /// Whether this source supports seeking (and therefore gets rewound to the
    /// start before a comparison).
    fn can_seek(&self) -> bool;

    /// Returns the current position, if seekable.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from the underlying seek.
    fn position(&mut self) -> io::Result<u64>;

    /// Sets the current position (only meaningful when seekable).
    ///
    /// # Errors
    ///
    /// Returns any I/O error from the underlying seek.
    fn set_position(&mut self, pos: u64) -> io::Result<()>;

    /// Returns the total length, if seekable.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from the underlying seek.
    fn length(&mut self) -> io::Result<u64>;
}

/// Blanket impl for anything that is both `Read` and `Seek` (the seekable case).
impl<T: Read + Seek> SeekableRead for T {
    fn can_seek(&self) -> bool {
        true
    }

    fn position(&mut self) -> io::Result<u64> {
        self.stream_position()
    }

    fn set_position(&mut self, pos: u64) -> io::Result<()> {
        self.seek(SeekFrom::Start(pos))?;
        Ok(())
    }

    fn length(&mut self) -> io::Result<u64> {
        let current = self.stream_position()?;
        let end = self.seek(SeekFrom::End(0))?;
        self.seek(SeekFrom::Start(current))?;
        Ok(end)
    }
}

/// Reads up to `buf.len()` bytes, looping until the buffer is filled or EOF is
/// reached — the equivalent of C# `ReadAtLeast(..., throwOnEndOfStream: false)`.
/// Returns the number of bytes read (0 only at immediate EOF).
fn read_at_least<R: Read + ?Sized>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Determines whether two byte sources are identical.
///
/// Seekable sources are rewound to position 0 on entry; non-seekable sources are
/// compared from their current position. Positions are not restored.
///
/// # Errors
///
/// Returns any I/O error from either source.
pub fn is_stream_identical<A, B>(a: &mut A, b: &mut B) -> io::Result<bool>
where
    A: SeekableRead + ?Sized,
    B: SeekableRead + ?Sized,
{
    let a_can_seek = a.can_seek();
    let b_can_seek = b.can_seek();

    if a_can_seek {
        a.set_position(0)?;
    }
    if b_can_seek {
        b.set_position(0)?;
    }

    if a_can_seek && b_can_seek && a.length()? != b.length()? {
        return Ok(false);
    }

    let mut buf_a = vec![0u8; STREAM_COMPARISON_BUFFER_SIZE];
    let mut buf_b = vec![0u8; STREAM_COMPARISON_BUFFER_SIZE];
    loop {
        let read_a = read_at_least(a, &mut buf_a)?;
        let read_b = read_at_least(b, &mut buf_b)?;

        if read_a != read_b {
            return Ok(false);
        }
        if read_a == 0 {
            return Ok(true);
        }
        if buf_a[..read_a] != buf_b[..read_b] {
            return Ok(false);
        }
    }
}

/// Determines whether a seekable stream is byte-identical to a file on disk.
///
/// The stream is compared from the beginning (position reset to 0 on entry) and
/// restored to its original value afterward.
///
/// # Errors
///
/// Returns [`StreamError::NotSeekable`] if `stream` cannot seek, or an
/// [`StreamError::Io`] for any underlying I/O failure.
pub fn is_file_identical<A, P>(stream: &mut A, path: P) -> Result<bool, StreamError>
where
    A: SeekableRead + ?Sized,
    P: AsRef<Path>,
{
    if !stream.can_seek() {
        return Err(StreamError::NotSeekable);
    }

    let original_position = stream.position()?;
    let result = (|| {
        stream.set_position(0)?;
        let mut file = std::fs::File::open(path)?;
        is_stream_identical(stream, &mut file)
    })();

    // Always restore, matching the C# `finally`.
    let restore = stream.set_position(original_position);

    let identical = result?;
    restore?;
    Ok(identical)
}

/// The encoding a .NET `StreamReader` reads a stream in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEncoding {
    /// UTF-8, with or without its byte-order mark.
    Utf8,
    /// UTF-16, little-endian.
    Utf16Le,
    /// UTF-16, big-endian.
    Utf16Be,
    /// UTF-32, little-endian.
    Utf32Le,
    /// UTF-32, big-endian.
    Utf32Be,
}

/// The longest byte-order mark (UTF-32's): how much of a stream a reader sees
/// before it knows the encoding.
const LONGEST_BOM: usize = 4;

/// The encoding the byte-order mark at the start of `bytes` names, and the
/// mark's length: UTF-8 and `0` when there is none. UTF-32LE's mark starts
/// with UTF-16LE's, so it is checked first.
fn detect_encoding(bytes: &[u8]) -> (TextEncoding, usize) {
    match bytes {
        [0xFF, 0xFE, 0x00, 0x00, ..] => (TextEncoding::Utf32Le, 4),
        [0x00, 0x00, 0xFE, 0xFF, ..] => (TextEncoding::Utf32Be, 4),
        [0xFF, 0xFE, ..] => (TextEncoding::Utf16Le, 2),
        [0xFE, 0xFF, ..] => (TextEncoding::Utf16Be, 2),
        [0xEF, 0xBB, 0xBF, ..] => (TextEncoding::Utf8, 3),
        _ => (TextEncoding::Utf8, 0),
    }
}

/// Decodes `bytes` (no byte-order mark) in `encoding`, an invalid sequence or
/// a trailing partial code unit becoming U+FFFD.
fn decode(encoding: TextEncoding, bytes: &[u8]) -> String {
    match encoding {
        TextEncoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        TextEncoding::Utf16Le => decode_utf16(bytes, u16::from_le_bytes),
        TextEncoding::Utf16Be => decode_utf16(bytes, u16::from_be_bytes),
        TextEncoding::Utf32Le => decode_utf32(bytes, u32::from_le_bytes),
        TextEncoding::Utf32Be => decode_utf32(bytes, u32::from_be_bytes),
    }
}

/// Decodes a text file's bytes as .NET's `new StreamReader(stream,
/// Encoding.UTF8)` and `File.ReadAllText(path)` do: a UTF-32, UTF-16 or UTF-8
/// byte-order mark picks the encoding and is dropped, anything else is UTF-8,
/// and a sequence that is invalid in that encoding becomes U+FFFD. It never
/// fails, so a Latin-1 or UTF-16 sidecar still reads.
#[must_use]
pub fn decode_stream_text(mut bytes: Vec<u8>) -> String {
    match detect_encoding(&bytes) {
        (TextEncoding::Utf8, bom) => {
            bytes.drain(..bom);
            String::from_utf8(bytes)
                .unwrap_or_else(|invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned())
        }
        (encoding, bom) => decode(encoding, &bytes[bom..]),
    }
}

/// Reads a byte stream line by line, a chunk at a time, as .NET's
/// `new StreamReader(stream)` read through `ReadLineAsync` does
/// (`StreamExtensions.ReadAllLinesAsync`), so a caller holds one line rather
/// than the whole stream.
///
/// The text is decoded as [`decode_stream_text`] decodes it: a UTF-32, UTF-16
/// or UTF-8 byte-order mark at the start picks the encoding and is dropped,
/// anything else is UTF-8, and an invalid sequence becomes U+FFFD. A line ends
/// at `\n`, `\r` or `\r\n` and does not include it; the last line needs no
/// terminator, and a terminator at the very end opens no empty line. A line
/// has no length limit, as upstream's has none: a stream that never ends a
/// line grows the current one, as `ReadLine`'s buffer grows.
#[derive(Debug, Default)]
pub struct LineReader {
    /// The encoding, once the stream's start has named it.
    encoding: Option<TextEncoding>,
    /// Bytes not yet decoded: the stream's first bytes until the encoding is
    /// known, then a UTF-16/32 code unit (or a surrogate pair) a chunk cut.
    pending: Vec<u8>,
    /// The line so far, as UTF-8, still raw (perhaps invalid) for a UTF-8
    /// stream: it is decoded once whole, so a chunk may cut a character.
    line: Vec<u8>,
    /// Whether the last character read was a `\r`, so a `\n` next is part of
    /// the same terminator.
    after_cr: bool,
}

impl LineReader {
    /// A reader at the start of a stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `chunk`, the stream's next bytes, calling `on_line` with each
    /// line it completes.
    pub fn push(&mut self, chunk: &[u8], mut on_line: impl FnMut(&str)) {
        let mut chunk = chunk;
        let encoding = if let Some(encoding) = self.encoding {
            encoding
        } else {
            let take = (LONGEST_BOM - self.pending.len()).min(chunk.len());
            self.pending.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.pending.len() < LONGEST_BOM {
                return;
            }
            self.settle(&mut on_line)
        };
        self.feed(encoding, chunk, &mut on_line);
    }

    /// Ends the stream, calling `on_line` with the last line when it holds
    /// anything. A code unit the stream cut short is U+FFFD.
    pub fn finish(mut self, mut on_line: impl FnMut(&str)) {
        let encoding = match self.encoding {
            Some(encoding) => encoding,
            None => self.settle(&mut on_line),
        };
        let rest = std::mem::take(&mut self.pending);
        if !rest.is_empty() {
            self.split(decode(encoding, &rest).as_bytes(), &mut on_line);
        }
        if !self.line.is_empty() {
            on_line(&String::from_utf8_lossy(&self.line));
        }
    }

    /// Picks the encoding from the stream's first bytes and reads what follows
    /// the byte-order mark.
    fn settle(&mut self, on_line: &mut impl FnMut(&str)) -> TextEncoding {
        let start = std::mem::take(&mut self.pending);
        let (encoding, bom) = detect_encoding(&start);
        self.encoding = Some(encoding);
        self.feed(encoding, &start[bom..], on_line);
        encoding
    }

    /// Reads `bytes` in `encoding`.
    fn feed(&mut self, encoding: TextEncoding, bytes: &[u8], on_line: &mut impl FnMut(&str)) {
        if encoding == TextEncoding::Utf8 {
            // `\r` and `\n` never occur inside a UTF-8 sequence, so the raw
            // bytes split as the decoded text would.
            self.split(bytes, on_line);
            return;
        }
        self.pending.extend_from_slice(bytes);
        let width = if matches!(encoding, TextEncoding::Utf16Le | TextEncoding::Utf16Be) {
            2
        } else {
            4
        };
        let mut whole = self.pending.len() - self.pending.len() % width;
        // A high surrogate at the end waits for its low half.
        if width == 2 && whole >= 2 {
            let last = [self.pending[whole - 2], self.pending[whole - 1]];
            let unit = if encoding == TextEncoding::Utf16Le {
                u16::from_le_bytes(last)
            } else {
                u16::from_be_bytes(last)
            };
            if (0xD800..0xDC00).contains(&unit) {
                whole -= 2;
            }
        }
        let text = decode(encoding, &self.pending[..whole]);
        self.pending.drain(..whole);
        self.split(text.as_bytes(), on_line);
    }

    /// Adds UTF-8 `bytes` to the line, calling `on_line` at each terminator.
    fn split(&mut self, mut bytes: &[u8], on_line: &mut impl FnMut(&str)) {
        if self.after_cr
            && let Some((&first, rest)) = bytes.split_first()
        {
            self.after_cr = false;
            if first == b'\n' {
                bytes = rest;
            }
        }
        while let Some(end) = bytes.iter().position(|&b| b == b'\r' || b == b'\n') {
            if self.line.is_empty() {
                on_line(&String::from_utf8_lossy(&bytes[..end]));
            } else {
                self.line.extend_from_slice(&bytes[..end]);
                on_line(&String::from_utf8_lossy(&self.line));
                self.line.clear();
            }
            let cr = bytes[end] == b'\r';
            bytes = &bytes[end + 1..];
            if cr {
                match bytes.split_first() {
                    Some((b'\n', rest)) => bytes = rest,
                    Some(_) => {}
                    None => self.after_cr = true,
                }
            }
        }
        self.line.extend_from_slice(bytes);
    }
}

/// UTF-16 code units in `unit`'s byte order; a lone surrogate or a trailing
/// odd byte becomes U+FFFD.
fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let (units, tail) = bytes.as_chunks::<2>();
    let mut text: String = char::decode_utf16(units.iter().map(|&pair| unit(pair)))
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if !tail.is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

/// UTF-32 code points in `unit`'s byte order; an invalid one or a trailing
/// partial unit becomes U+FFFD.
fn decode_utf32(bytes: &[u8], unit: fn([u8; 4]) -> u32) -> String {
    let (units, tail) = bytes.as_chunks::<4>();
    let mut text: String = units
        .iter()
        .map(|&quad| char::from_u32(unit(quad)).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if !tail.is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::io::Cursor;

    #[rstest::rstest]
    #[case::plain(b"Caf\xC3\xA9", "Café")]
    #[case::utf8_bom(b"\xEF\xBB\xBFCaf\xC3\xA9", "Café")]
    #[case::latin1(b"Caf\xE9", "Caf\u{FFFD}")]
    #[case::utf16_le(b"\xFF\xFEC\x00a\x00f\x00\xE9\x00", "Café")]
    #[case::utf16_be(b"\xFE\xFF\x00C\x00a\x00f\x00\xE9", "Café")]
    #[case::utf16_lone_surrogate(b"\xFF\xFE\x00\xD8C\x00", "\u{FFFD}C")]
    #[case::utf16_odd_byte(b"\xFF\xFEC\x00a", "C\u{FFFD}")]
    #[case::utf32_le(b"\xFF\xFE\x00\x00\xE9\x00\x00\x00\x3B\xF6\x01\x00", "é😻")]
    #[case::utf32_be(b"\x00\x00\xFE\xFF\x00\x00\x00\xE9\x00\x01\xF6\x3B", "é😻")]
    #[case::utf32_invalid(b"\xFF\xFE\x00\x00\x00\x00\x11\x00", "\u{FFFD}")]
    #[case::empty(b"", "")]
    fn decode_stream_text_matches_the_dotnet_reader(#[case] bytes: &[u8], #[case] expected: &str) {
        assert_eq!(decode_stream_text(bytes.to_vec()), expected);
    }

    /// Every line a [`LineReader`] reads from `chunks`.
    fn read_lines(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut reader = LineReader::new();
        for chunk in chunks {
            reader.push(chunk, |line| lines.push(line.to_owned()));
        }
        reader.finish(|line| lines.push(line.to_owned()));
        lines
    }

    /// `text` as UTF-16 in `unit`'s byte order behind its byte-order mark.
    fn utf16(text: &str, bom: [u8; 2], unit: fn(u16) -> [u8; 2]) -> Vec<u8> {
        let mut bytes = bom.to_vec();
        bytes.extend(text.encode_utf16().flat_map(unit));
        bytes
    }

    /// `text` as UTF-32 in `unit`'s byte order behind its byte-order mark.
    fn utf32(text: &str, bom: [u8; 4], unit: fn(u32) -> [u8; 4]) -> Vec<u8> {
        let mut bytes = bom.to_vec();
        bytes.extend(text.chars().flat_map(|c| unit(u32::from(c))));
        bytes
    }

    /// Reads `stream` as cut into chunks at each `|`, then one byte at a time
    /// (which cuts every terminator, character and code unit), and checks
    /// both read `expected`.
    fn assert_lines(stream: &[u8], expected: &[&str]) {
        let chunks: Vec<&[u8]> = stream.split(|&b| b == b'|').collect();
        assert_eq!(read_lines(&chunks), expected, "as chunked");
        let whole = chunks.concat();
        let one_by_one: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(read_lines(&one_by_one), expected, "one byte at a time");
    }

    /// `StreamReader.ReadLine`: a line ends at `\n`, `\r` or `\r\n`, wherever
    /// the chunks are cut (each `|`), and the stream is decoded as
    /// [`decode_stream_text`] decodes it.
    #[rstest::rstest]
    #[case::lf(b"a\nb\n", &["a", "b"])]
    #[case::crlf(b"a\r\nb\r\n", &["a", "b"])]
    #[case::cr_only(b"a\rb\r", &["a", "b"])]
    #[case::crlf_split(b"a\r|\nb", &["a", "b"])]
    #[case::cr_then_cr(b"a\r|\rb", &["a", "", "b"])]
    #[case::lf_cr(b"a\n\rb", &["a", "", "b"])]
    #[case::blank_lines(b"a\n\n\r\n\nb", &["a", "", "", "", "b"])]
    #[case::line_split(b"ab||c|d\n", &["abcd"])]
    #[case::no_final_terminator(b"a\nb", &["a", "b"])]
    #[case::only_a_terminator(b"\r\n", &[""])]
    #[case::empty(b"", &[])]
    #[case::shorter_than_a_bom(b"a", &["a"])]
    #[case::utf8_bom(b"\xEF\xBB\xBF#EXTINF:-1,A\n", &["#EXTINF:-1,A"])]
    #[case::utf8_bom_split(b"\xEF|\xBB|\xBFa", &["a"])]
    #[case::only_a_utf8_bom(b"\xEF\xBB\xBF", &[])]
    #[case::bom_after_the_start(b"a\n\xEF\xBB\xBFb", &["a", "\u{FEFF}b"])]
    #[case::character_split(b"Caf\xC3|\xA9\n", &["Café"])]
    #[case::latin1(b"Caf\xE9\n", &["Caf\u{FFFD}"])]
    #[case::cut_before_a_terminator(b"\xE2\x82\nb", &["\u{FFFD}", "b"])]
    #[case::utf16_lone_surrogate(b"\xFF\xFE\x00\xD8C\x00\n\x00", &["\u{FFFD}C"])]
    #[case::utf16_trailing_surrogate(b"\xFF\xFEC\x00\x00\xD8", &["C\u{FFFD}"])]
    #[case::utf16_odd_byte(b"\xFF\xFEC\x00a", &["C\u{FFFD}"])]
    #[case::utf16_only_a_bom(b"\xFF\xFE", &[])]
    #[case::utf32_invalid(b"\xFF\xFE\x00\x00\x00\x00\x11\x00\x41\x00", &["\u{FFFD}\u{FFFD}"])]
    fn the_line_reader_matches_the_dotnet_stream_reader(
        #[case] stream: &[u8],
        #[case] expected: &[&str],
    ) {
        assert_lines(stream, expected);
    }

    /// UTF-16 and UTF-32 streams, surrogate pairs included, read whole and one
    /// byte at a time.
    #[rstest::rstest]
    #[case::utf16_le(utf16("a\r\n😻\rb", [0xFF, 0xFE], u16::to_le_bytes), &["a", "😻", "b"])]
    #[case::utf16_be(utf16("a\n😻\n", [0xFE, 0xFF], u16::to_be_bytes), &["a", "😻"])]
    #[case::utf32_le(
        utf32("é\r\n😻", [0xFF, 0xFE, 0x00, 0x00], u32::to_le_bytes),
        &["é", "😻"]
    )]
    #[case::utf32_be(
        utf32("é\r😻\n", [0x00, 0x00, 0xFE, 0xFF], u32::to_be_bytes),
        &["é", "😻"]
    )]
    fn the_line_reader_reads_utf16_and_utf32(#[case] stream: Vec<u8>, #[case] expected: &[&str]) {
        assert!(!stream.contains(&b'|'), "no chunk cut is meant here");
        assert_lines(&stream, expected);
    }

    /// Read whole, the lines are those of [`decode_stream_text`]'s text.
    #[rstest::rstest]
    #[case(b"\xEF\xBB\xBFone\r\ntwo\nthree\n")]
    #[case(b"\xFF\xFEo\x00n\x00e\x00\r\x00\n\x00t\x00w\x00o\x00")]
    #[case(b"Caf\xE9\n\nend")]
    fn the_line_reader_decodes_as_decode_stream_text(#[case] bytes: &[u8]) {
        let text = decode_stream_text(bytes.to_vec());
        let expected: Vec<&str> = text.lines().collect();
        assert_eq!(read_lines(&[bytes]), expected);
    }

    /// A non-seekable reader over an in-memory buffer.
    struct NonSeekableReadStream {
        inner: Cursor<Vec<u8>>,
    }

    impl NonSeekableReadStream {
        fn new(data: &[u8]) -> Self {
            Self {
                inner: Cursor::new(data.to_vec()),
            }
        }
    }

    impl Read for NonSeekableReadStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl SeekableRead for NonSeekableReadStream {
        fn can_seek(&self) -> bool {
            false
        }
        fn position(&mut self) -> io::Result<u64> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
        }
        fn set_position(&mut self, _pos: u64) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
        }
        fn length(&mut self) -> io::Result<u64> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
        }
    }

    /// A non-seekable reader that returns at most `max_read_size` bytes per read,
    /// exercising the short-read handling.
    struct ShortReadingNonSeekableStream {
        inner: Cursor<Vec<u8>>,
        max_read_size: usize,
    }

    impl ShortReadingNonSeekableStream {
        fn new(data: &[u8], max_read_size: usize) -> Self {
            Self {
                inner: Cursor::new(data.to_vec()),
                max_read_size,
            }
        }
    }

    impl Read for ShortReadingNonSeekableStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let cap = buf.len().min(self.max_read_size);
            self.inner.read(&mut buf[..cap])
        }
    }

    impl SeekableRead for ShortReadingNonSeekableStream {
        fn can_seek(&self) -> bool {
            false
        }
        fn position(&mut self) -> io::Result<u64> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
        }
        fn set_position(&mut self, _pos: u64) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
        }
        fn length(&mut self) -> io::Result<u64> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
        }
    }

    fn temp_path() -> std::path::PathBuf {
        temp_dir().join(format!("ferrofin-stream-{}.tmp", uuid::Uuid::new_v4()))
    }

    #[test]
    fn is_stream_identical_seekable_different_lengths_returns_false() {
        let mut a = Cursor::new(vec![1u8, 2, 3]);
        let mut b = Cursor::new(vec![1u8, 2, 3, 4]);
        assert!(!is_stream_identical(&mut a, &mut b).unwrap());
    }

    #[test]
    fn is_stream_identical_non_seekable_identical_streams_returns_true() {
        let mut a = NonSeekableReadStream::new(&[1, 2, 3, 4]);
        let mut b = NonSeekableReadStream::new(&[1, 2, 3, 4]);
        assert!(is_stream_identical(&mut a, &mut b).unwrap());
    }

    #[test]
    fn is_stream_identical_non_seekable_different_streams_returns_false() {
        let mut a = NonSeekableReadStream::new(&[1, 2, 3, 4]);
        let mut b = NonSeekableReadStream::new(&[1, 2, 9, 4]);
        assert!(!is_stream_identical(&mut a, &mut b).unwrap());
    }

    #[test]
    fn is_file_identical_non_seekable_stream_returns_not_seekable_error() {
        let path = temp_path();
        std::fs::write(&path, [1u8, 2, 3, 4]).unwrap();
        let mut stream = NonSeekableReadStream::new(&[1, 2, 3, 4]);
        let result = is_file_identical(&mut stream, &path);
        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(StreamError::NotSeekable)));
    }

    #[test]
    fn is_file_identical_uses_start_of_stream_and_restores_position_on_match() {
        let path = temp_path();
        let bytes = [10u8, 20, 30, 40, 50];
        std::fs::write(&path, bytes).unwrap();

        let mut stream = Cursor::new(bytes.to_vec());
        stream.set_position(3);

        let result = is_file_identical(&mut stream, &path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(result);
        assert_eq!(3, stream.position());
    }

    #[test]
    fn is_file_identical_restores_position_on_mismatch() {
        let path = temp_path();
        std::fs::write(&path, [10u8, 20, 30, 40, 99]).unwrap();

        let mut stream = Cursor::new(vec![10u8, 20, 30, 40, 50]);
        stream.set_position(2);

        let result = is_file_identical(&mut stream, &path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(!result);
        assert_eq!(2, stream.position());
    }

    #[test]
    fn is_stream_identical_both_seekable_non_zero_positions_seeks_to_start() {
        let mut a = Cursor::new(vec![1u8, 2, 3, 4, 5]);
        let mut b = Cursor::new(vec![1u8, 2, 3, 4, 5]);
        a.set_position(3);
        b.set_position(1);
        assert!(is_stream_identical(&mut a, &mut b).unwrap());
    }

    #[test]
    fn is_stream_identical_non_seekable_short_reads_identical_returns_true() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let mut a = ShortReadingNonSeekableStream::new(&data, 3);
        let mut b = ShortReadingNonSeekableStream::new(&data, 5);
        assert!(is_stream_identical(&mut a, &mut b).unwrap());
    }

    #[test]
    fn is_stream_identical_non_seekable_short_reads_different_lengths_returns_false() {
        let mut a = ShortReadingNonSeekableStream::new(&[1, 2, 3, 4], 3);
        let mut b = ShortReadingNonSeekableStream::new(&[1, 2, 3, 4, 5], 5);
        assert!(!is_stream_identical(&mut a, &mut b).unwrap());
    }

    #[test]
    fn read_all_lines_splits_on_newlines() {
        let data = b"first\nsecond\nthird";
        let lines = read_all_lines(&data[..]).unwrap();
        assert_eq!(vec!["first", "second", "third"], lines);
    }
}
