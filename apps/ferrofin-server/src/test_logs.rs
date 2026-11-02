//! Test-only log capture: a scoped `fmt` subscriber writing into memory, for
//! tests that assert what a code path logs and at which level.

use std::sync::{Arc, Mutex};

/// An in-memory log sink. [`install`](Self::install) makes it the current
/// thread's subscriber (at `debug` and above) until the guard drops; a
/// `#[tokio::test]` runs on that one thread, so awaited code logs into it.
#[derive(Clone, Default)]
pub(crate) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    /// Captures this thread's events until the returned guard drops.
    pub(crate) fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .finish(),
        )
    }

    /// Everything captured so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// The captured lines at `level` (`"WARN"`, `"DEBUG"`, …).
    pub(crate) fn lines_at(&self, level: &str) -> Vec<String> {
        let marker = format!(" {level} ");
        self.text()
            .lines()
            .filter(|line| line.contains(&marker))
            .map(str::to_owned)
            .collect()
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
