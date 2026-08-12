//! Port of `Emby.Naming.AudioBook.AudioBookNameParser`.

use fancy_regex::Regex;

use crate::audiobook::AudioBookNameParserResult;
use crate::common::NamingOptions;

/// Retrieves name and year from a previously-determined audiobook name.
pub struct AudioBookNameParser<'a> {
    options: &'a NamingOptions,
}

impl<'a> AudioBookNameParser<'a> {
    /// Creates a new [`AudioBookNameParser`].
    #[must_use]
    pub fn new(options: &'a NamingOptions) -> Self {
        Self { options }
    }

    /// Parses name and year from the audiobook `name`.
    #[must_use]
    pub fn parse(&self, name: &str) -> AudioBookNameParserResult {
        let mut result = AudioBookNameParserResult::default();

        for expression in &self.options.audio_book_names_expressions {
            let Ok(regex) = Regex::new(&format!("(?i){expression}")) else {
                continue;
            };
            let Ok(Some(captures)) = regex.captures(name) else {
                continue;
            };

            if result.name.is_none() {
                result.name = captures.name("name").map(|m| m.as_str().to_string());
            }

            if result.year.is_none() {
                result.year = captures
                    .name("year")
                    .and_then(|m| m.as_str().parse::<i32>().ok());
            }
        }

        if result.name.as_deref().unwrap_or("").is_empty() {
            result.name = Some(name.to_string());
        }

        result
    }
}
