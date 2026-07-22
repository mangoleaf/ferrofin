//! A minimal logging seam — the port of the `ILogger` dependency.
//!
//! The upstream code takes an `ILogger` and a handful of tests assert on exact
//! warning *substrings* it emits. Rather than pull in `tracing` (and make those
//! assertions non-deterministic), the port models the single method it needs as
//! a small trait. Production callers pass [`NullLogger`]; tests pass a capturing
//! implementation.

/// Sink for the warning/diagnostic messages emitted by the networking code.
///
/// Only the `warn` level is asserted by the ported tests; other C# log levels
/// (`Debug`/`Information`/`Error`) are not observable behavior and are dropped.
pub trait Logger {
    /// Records a warning-level message.
    fn warn(&self, message: &str);
}

/// A [`Logger`] that discards everything (`NullLogger<T>`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NullLogger;

impl Logger for NullLogger {
    fn warn(&self, _message: &str) {}
}
