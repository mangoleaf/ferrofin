//! A minimal logging seam — the port of the `ILogger` dependency.
//!
//! The upstream code takes an `ILogger` and a handful of tests assert on exact
//! warning *substrings* it emits. The port models the levels it needs as a
//! small trait, so those assertions stay deterministic. The server's shared
//! manager gets a [`TracingLogger`]; tests pass [`NullLogger`] or a capturing
//! implementation.

/// Sink for the warning/diagnostic messages emitted by the networking code.
///
/// Upstream's `Debug` messages are not ported here.
pub trait Logger: Send + Sync {
    /// Records an information-level message (`LogInformation`).
    fn info(&self, message: &str);
    /// Records a warning-level message (`LogWarning`).
    fn warn(&self, message: &str);
    /// Records an error-level message (`LogError`).
    fn error(&self, message: &str);
}

/// A [`Logger`] that discards everything (`NullLogger<T>`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NullLogger;

impl Logger for NullLogger {
    fn info(&self, _message: &str) {}
    fn warn(&self, _message: &str) {}
    fn error(&self, _message: &str) {}
}

/// A [`Logger`] that writes to the server log through `tracing`.
///
/// A warning or an error quotes the configured entry, which the admin typed,
/// so it goes in a Debug-formatted field, never the event's message
/// (LOGGING.md rule 6). An information line is the manager's own summary of
/// what it parsed, so it is the message, as upstream words it.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingLogger;

impl Logger for TracingLogger {
    fn info(&self, message: &str) {
        tracing::info!("{message}");
    }
    fn warn(&self, message: &str) {
        tracing::warn!(detail = ?message, "network setting ignored");
    }
    fn error(&self, message: &str) {
        tracing::error!(detail = ?message, "network setting ignored");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{Logger, TracingLogger};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Each level stays its own, and the message is an escaped field: a line
    /// break in what the admin typed cannot start a log line of its own.
    #[test]
    fn tracing_logger_keeps_the_level_and_escapes_the_message() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            TracingLogger.warn("Invalid subnet 'a\nb' will be ignored.");
            TracingLogger.error("Unable to parse bind override: x");
            TracingLogger.info("Remote IP filter is Allowlist");
        });

        let text = String::from_utf8(captured.0.lock().expect("log").clone()).expect("UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].trim_start().starts_with("WARN"), "{text}");
        assert!(
            lines[0].ends_with(
                r#"network setting ignored detail="Invalid subnet 'a\nb' will be ignored.""#
            ),
            "{text}"
        );
        assert!(lines[1].starts_with("ERROR"), "{text}");
        assert!(
            lines[1]
                .ends_with(r#"network setting ignored detail="Unable to parse bind override: x""#),
            "{text}"
        );
        assert!(lines[2].trim_start().starts_with("INFO"), "{text}");
        assert!(
            lines[2].ends_with("Remote IP filter is Allowlist"),
            "{text}"
        );
    }
}
