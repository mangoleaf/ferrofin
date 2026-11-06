//! Records the SQL statements one call runs, for the tests that pin how many
//! reads a path makes.
//!
//! A process-wide subscriber keeps the `sqlx::query` events emitted inside one
//! call's span (the SQLite worker enters the caller's span), tagged with the
//! recording's id: `cargo test` runs a binary's tests on threads of one process.

use std::sync::Mutex;

use tracing::Instrument as _;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;

/// The span one recorded call runs in.
const SPAN: &str = "recorded_statements";

/// The statements recorded inside [`SPAN`], with the recording's id.
static STATEMENTS: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());

/// Records the text of every `sqlx::query` event emitted inside [`SPAN`], with
/// the span's `recording` field.
struct Statements;

/// The `recording` field of a [`SPAN`].
struct Recording(u64);

impl<S> tracing_subscriber::Layer<S> for Statements
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if attrs.metadata().name() != SPAN {
            return;
        }
        let mut recording = RecordingField(None);
        attrs.record(&mut recording);
        if let (Some(span), Some(recording)) = (ctx.span(id), recording.0) {
            span.extensions_mut().insert(Recording(recording));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let Some(recording) = ctx.event_scope(event).and_then(|mut scope| {
            scope.find_map(|span| span.extensions().get::<Recording>().map(|r| r.0))
        }) else {
            return;
        };
        let mut text = StatementText(String::new());
        event.record(&mut text);
        STATEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((recording, text.0));
    }
}

/// Reads a span's `recording` field.
struct RecordingField(Option<u64>);

impl tracing::field::Visit for RecordingField {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "recording" {
            self.0 = Some(value);
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

/// sqlx's `summary` (the statement's start) and `db.statement` (the whole
/// statement, when the summary is cut short).
struct StatementText(String);

impl tracing::field::Visit for StatementText {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if matches!(field.name(), "summary" | "db.statement") {
            self.0.push_str(value);
            self.0.push(' ');
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if matches!(field.name(), "summary" | "db.statement") {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{value:?} ");
        }
    }
}

/// Installs [`Statements`] as the process's subscriber, once.
pub fn record_statements() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Statements))
            .expect("no other subscriber in this binary");
    });
}

/// Runs `call` inside a fresh [`SPAN`] and returns its output and the text of
/// every statement it ran, in order. [`record_statements`] must have run.
pub async fn statements_of<T>(call: impl std::future::Future<Output = T>) -> (T, Vec<String>) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let recording = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let output = call.instrument(tracing::info_span!(SPAN, recording)).await;
    let statements = STATEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(id, _)| *id == recording)
        .map(|(_, statement)| statement.clone())
        .collect();
    (output, statements)
}
