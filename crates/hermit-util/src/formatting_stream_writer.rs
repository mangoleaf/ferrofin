//! Port of `FormattingStreamWriter.cs`.
//!
//! The C# class existed to force invariant-culture number formatting regardless
//! of the ambient `Thread.CurrentCulture` (so `3.14159` never renders as
//! `3,14159`). Rust's `Display`/`write!` formatting is already locale-invariant,
//! so this is a thin wrapper over an underlying [`std::io::Write`] that writes
//! formatted values with the standard (invariant) formatter. The
//! `IFormatProvider` plumbing has no Rust analog.

use std::io::{self, Write};

/// A writer that formats values with the locale-invariant standard formatter.
pub struct FormattingStreamWriter<W: Write> {
    inner: W,
}

impl<W: Write> FormattingStreamWriter<W> {
    /// Creates a new writer wrapping `inner`.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Writes pre-rendered formatted arguments to the underlying writer.
    ///
    /// Use with the standard [`format_args!`]/[`write!`] machinery, which is
    /// already invariant-culture. This is the analog of the C# `Write("{0}", v)`
    /// exercised by the parity test.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from the underlying writer.
    pub fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        self.inner.write_fmt(args)
    }

    /// Consumes the wrapper and returns the underlying writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for FormattingStreamWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::approx_constant)] // 3.14159 is the literal from the C# parity test, not PI-as-a-constant
    fn invariant_number_formatting() {
        // Equivalent to writing "{0}" with 3.14159 under the invariant culture;
        // Rust's f64 Display is already invariant so this renders "3.14159".
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut txt = FormattingStreamWriter::new(&mut buf);
            write!(txt, "{}", 3.14159).expect("write should succeed");
        }
        assert_eq!("3.14159", String::from_utf8(buf).expect("utf8"));
    }

    #[test]
    fn write_and_flush_delegate_to_inner() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut txt = FormattingStreamWriter::new(&mut buf);
            let n = txt.write(b"hello").expect("write should succeed");
            assert_eq!(5, n);
            txt.flush().expect("flush should succeed");
        }
        assert_eq!(b"hello".as_slice(), buf.as_slice());
    }

    #[test]
    fn into_inner_returns_underlying_writer() {
        let buf: Vec<u8> = Vec::new();
        let mut txt = FormattingStreamWriter::new(buf);
        write!(txt, "{}", 42).expect("write should succeed");
        let inner = txt.into_inner();
        assert_eq!("42", String::from_utf8(inner).expect("utf8"));
    }
}
