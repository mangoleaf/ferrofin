//! Captures the log lines a unit test emits, for tests that count them.

use std::sync::Arc;

/// The bytes a test's scoped `tracing_subscriber::fmt` subscriber wrote.
#[derive(Clone, Default)]
pub(crate) struct LogBuffer(pub(crate) Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs the process-wide fallback subscriber every capturing test needs.
pub(crate) fn init_log_capture() {
    // Cargo runs other provider tests on threads without our scoped
    // subscriber. If one of those threads first reaches a log callsite,
    // tracing can cache it as disabled for the capturing test too. Keep
    // a DEBUG-enabled fallback alive for the entire test process; each
    // test's scoped subscriber still writes only to its own LogBuffer.
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(std::io::sink)
                .finish(),
        )
        .expect("provider tests install their fallback subscriber only once");
    });
}

#[test]
fn log_capture_survives_first_use_on_another_thread() {
    fn emit() {
        tracing::warn!("provider log capture regression");
    }

    init_log_capture();
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(logs.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        // The first visit to this shared callsite comes from outside the
        // scoped capture, as when another provider test runs concurrently.
        std::thread::spawn(emit).join().unwrap();
        emit();
    });
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("provider log capture regression").count(),
        1,
        "capture must retain its own event and exclude the other thread:\n{logs}"
    );
}
