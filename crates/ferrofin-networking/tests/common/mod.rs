//! Shared test helpers: a [`Logger`] that captures what it is given so the ported
//! `LogInvalidSubnet` substring assertions can be checked deterministically
//! (the Moq `logger.Verify(...Contains...)` oracle).
//!
//! A `Mutex`, not a `RefCell`: [`Logger`] is `Send + Sync` so that
//! `NetworkManager` can live in the server's shared state.

use std::sync::Mutex;

use ferrofin_networking::Logger;

/// A [`Logger`] that records every message it receives.
#[derive(Default)]
pub struct CapturingLogger {
    infos: Mutex<Vec<String>>,
    warnings: Mutex<Vec<String>>,
    errors: Mutex<Vec<String>>,
}

impl CapturingLogger {
    /// Creates an empty capturing logger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorded information messages.
    #[must_use]
    pub fn infos(&self) -> Vec<String> {
        self.infos.lock().expect("infos lock").clone()
    }

    /// The recorded warning messages.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().expect("warnings lock").clone()
    }

    /// The recorded error messages.
    #[must_use]
    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().expect("errors lock").clone()
    }

    /// Number of warnings that contain `needle`.
    #[must_use]
    pub fn warning_count_containing(&self, needle: &str) -> usize {
        self.warnings
            .lock()
            .expect("warnings lock")
            .iter()
            .filter(|w| w.contains(needle))
            .count()
    }

    /// Total number of warnings recorded.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.warnings.lock().expect("warnings lock").len()
    }
}

impl Logger for CapturingLogger {
    fn info(&self, message: &str) {
        self.infos
            .lock()
            .expect("infos lock")
            .push(message.to_owned());
    }

    fn warn(&self, message: &str) {
        self.warnings
            .lock()
            .expect("warnings lock")
            .push(message.to_owned());
    }

    fn error(&self, message: &str) {
        self.errors
            .lock()
            .expect("errors lock")
            .push(message.to_owned());
    }
}
