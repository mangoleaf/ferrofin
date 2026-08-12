//! Shared test helpers: a [`Logger`] that captures warnings so the ported
//! `LogInvalidSubnet` substring assertions can be checked deterministically
//! (the Moq `logger.Verify(...Contains...)` oracle).

use std::cell::RefCell;

use ferrofin_networking::Logger;

/// A [`Logger`] that records every warning message it receives.
#[derive(Default)]
pub struct CapturingLogger {
    warnings: RefCell<Vec<String>>,
}

impl CapturingLogger {
    /// Creates an empty capturing logger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorded warning messages.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.borrow().clone()
    }

    /// Number of warnings that contain `needle`.
    #[must_use]
    pub fn warning_count_containing(&self, needle: &str) -> usize {
        self.warnings
            .borrow()
            .iter()
            .filter(|w| w.contains(needle))
            .count()
    }

    /// Total number of warnings recorded.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.warnings.borrow().len()
    }
}

impl Logger for CapturingLogger {
    fn warn(&self, message: &str) {
        self.warnings.borrow_mut().push(message.to_owned());
    }
}
