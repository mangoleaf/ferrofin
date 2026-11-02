//! The text log's record lines: no text inside one event may begin a line of
//! its own.
//!
//! The plain-text log that `/System/Logs` serves holds one record per line,
//! each starting with its timestamp at the left margin. A field or message
//! carrying a line break (a value from a request header, a plugin's text)
//! would otherwise write a line that reads as a record of its own: a forged
//! entry in the log an administrator reviews. JSON escapes line breaks itself;
//! the text sinks write through [`RecordLines`], which turns an event's
//! interior line feeds into indented continuation lines and its carriage
//! returns into `\r`. Multi-line text (an ffmpeg error, a panic) stays
//! readable, and nothing inside an event can start at the left margin (owner
//! decision 2026-10-24, over escaping every break onto one line).
//!
//! Only line feeds and carriage returns are handled. The formatter already
//! escapes the terminal controls (ESC, BEL, BS, FF, DEL and C1) in messages,
//! and the remaining controls and the Unicode line separators (U+2028,
//! U+2029) do not break a line in a plain-text viewer.

use std::borrow::Cow;
use std::io;

use tracing_subscriber::fmt::MakeWriter;

/// What follows each line feed inside an event: the continuation indent.
const CONTINUATION: &[u8] = b"\n    ";

/// A [`MakeWriter`] whose writers keep each event on lines of its own record
/// (see the [module docs](self)).
///
/// The text formatter hands its writer one whole event per `write_all`, so a
/// line feed is interior unless it ends the buffer.
#[derive(Debug, Clone)]
pub(crate) struct RecordLines<M>(pub(crate) M);

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RecordLines<M> {
    type Writer = RecordLinesWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RecordLinesWriter(self.0.make_writer())
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RecordLinesWriter(self.0.make_writer_for(meta))
    }
}

/// The writer [`RecordLines`] makes.
#[derive(Debug)]
pub(crate) struct RecordLinesWriter<W>(W);

impl<W: io::Write> io::Write for RecordLinesWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all(&neutralize(buf))?;
        // `write` reports the input it consumed (all of it), not the longer
        // output it wrote.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// `event` with every carriage return written as `\r` and every line feed
/// but a final one followed by [`CONTINUATION`].
fn neutralize(event: &[u8]) -> Cow<'_, [u8]> {
    let body = event.strip_suffix(b"\n").unwrap_or(event);
    if !body.iter().any(|&b| b == b'\n' || b == b'\r') {
        return Cow::Borrowed(event);
    }
    let mut out = Vec::with_capacity(event.len() + 16);
    for &byte in body {
        match byte {
            b'\n' => out.extend_from_slice(CONTINUATION),
            b'\r' => out.extend_from_slice(b"\\r"),
            other => out.push(other),
        }
    }
    if body.len() < event.len() {
        out.push(b'\n');
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[rstest::rstest]
    #[case::one_line(b"2026-10-24 INFO a b\n", b"2026-10-24 INFO a b\n")]
    #[case::interior_line_feed(b"a\nb\n", b"a\n    b\n")]
    #[case::carriage_return(b"a\rb\n", b"a\\rb\n")]
    #[case::crlf(b"a\r\nb\n", b"a\\r\n    b\n")]
    #[case::no_final_line_feed(b"a\nb", b"a\n    b")]
    fn interior_breaks_cannot_start_a_record(#[case] event: &[u8], #[case] written: &[u8]) {
        assert_eq!(&*neutralize(event), written);
    }

    /// A one-line event is written untouched, without a copy.
    #[test]
    fn a_one_line_event_is_borrowed() {
        assert!(matches!(neutralize(b"plain\n"), Cow::Borrowed(_)));
    }

    /// An in-memory sink for one subscriber.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// gl-154: a value from a request (a `Device` header URL-decoded into a
    /// line break, say) logged through the text sink stays inside its own
    /// record: every line it adds is indented, so none reads as a record.
    #[test]
    fn a_logged_line_break_cannot_forge_a_record() {
        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(RecordLines(move || writer.clone()))
            .finish();
        let device = "phone\n2026-10-24T00:00:00.000000Z ERROR ferrofin: forged\rx";
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(device = %device, "playback started");
            tracing::info!("next");
        });
        let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(
            lines[1].starts_with("    2026-10-24T00:00:00.000000Z ERROR"),
            "{text}"
        );
        assert!(lines[1].contains("forged\\rx"), "{text}");
        assert!(
            lines[2].contains("next") && !lines[2].starts_with(' '),
            "{text}"
        );
    }
}
