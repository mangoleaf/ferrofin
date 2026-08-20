//! Port of `Emby.Naming.AudioBook.AudioBookFilePathParser`.

use crate::audiobook::AudioBookFilePathParserResult;
use crate::common::NamingOptions;
use crate::path;

/// Extracts part and/or chapter numbers from an audiobook filename.
pub struct AudioBookFilePathParser<'a> {
    options: &'a NamingOptions,
}

impl<'a> AudioBookFilePathParser<'a> {
    /// Creates a new [`AudioBookFilePathParser`].
    #[must_use]
    pub fn new(options: &'a NamingOptions) -> Self {
        Self { options }
    }

    /// Parses part/chapter information from the filename at `path_str`.
    #[must_use]
    pub fn parse(&self, path_str: &str) -> AudioBookFilePathParserResult {
        let mut result = AudioBookFilePathParserResult::default();
        let file_name = path::file_name_without_extension(path_str);

        for regex in &self.options.audio_book_parts_regexes {
            let Ok(Some(captures)) = regex.captures(file_name) else {
                continue;
            };

            if result.chapter_number.is_none() {
                result.chapter_number = capture_i32(&captures, "chapter");
            }

            if result.part_number.is_none() {
                result.part_number = capture_i32(&captures, "part");
            }
        }

        result
    }
}

fn capture_i32(captures: &fancy_regex::Captures<'_>, name: &str) -> Option<i32> {
    captures.name(name)?.as_str().parse::<i32>().ok()
}
