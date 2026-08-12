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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::io::Cursor;

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
