//! Port of `Emby.Naming.Common.EpisodeExpression`.

use std::cell::OnceCell;

use fancy_regex::Regex;

/// Regular expression for parsing TV episodes.
///
/// The C# type lazily compiles the [`Regex`] on first access and resets it when
/// [`Self::set_expression`] is called; we mirror that with a [`OnceCell`].
// Four independent flags, one-for-one with the C# `EpisodeExpression` class.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct EpisodeExpression {
    expression: String,
    regex: OnceCell<Regex>,
    /// Indicates whether a date is expected in the expression.
    pub is_by_date: bool,
    /// Indicates whether the expression is optimistic.
    pub is_optimistic: bool,
    /// Indicates whether the expression is named.
    pub is_named: bool,
    /// Indicates whether the expression supports absolute episode numbers.
    pub supports_absolute_episode_numbers: bool,
    /// Optional list of date formats used for date parsing.
    pub date_time_formats: Vec<String>,
}

impl EpisodeExpression {
    /// Creates a new [`EpisodeExpression`].
    #[must_use]
    pub fn new(expression: impl Into<String>, by_date: bool) -> Self {
        Self {
            expression: expression.into(),
            regex: OnceCell::new(),
            is_by_date: by_date,
            is_optimistic: false,
            is_named: false,
            supports_absolute_episode_numbers: true,
            date_time_formats: Vec::new(),
        }
    }

    /// Returns the raw expression string.
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// Sets the raw expression string, invalidating the compiled regex.
    pub fn set_expression(&mut self, value: impl Into<String>) {
        self.expression = value.into();
        self.regex = OnceCell::new();
    }

    /// Returns the compiled [`Regex`], compiling it (case-insensitively) on
    /// first access.
    ///
    /// # Panics
    ///
    /// Panics if the expression is not a valid regex. All expressions in
    /// production come from the vendored `NamingOptions` tables and are valid;
    /// tests that construct arbitrary expressions supply valid regexes.
    #[must_use]
    pub fn regex(&self) -> &Regex {
        self.regex.get_or_init(|| {
            Regex::new(&format!("(?i){}", self.expression))
                .expect("EpisodeExpression pattern is a valid regex")
        })
    }
}
