//! Port of `Emby.Naming.AudioBook.AudioBookResolver`.

use crate::audiobook::{AudioBookFileInfo, AudioBookFilePathParser};
use crate::common::NamingOptions;
use crate::path;

/// Resolves specifics (path, container, part/chapter number) about an
/// audiobook file.
pub struct AudioBookResolver<'a> {
    options: &'a NamingOptions,
}

impl<'a> AudioBookResolver<'a> {
    /// Creates a new [`AudioBookResolver`].
    #[must_use]
    pub fn new(options: &'a NamingOptions) -> Self {
        Self { options }
    }

    /// Resolves specifics about the audiobook file at `path_str`.
    #[must_use]
    pub fn resolve(&self, path_str: &str) -> Option<AudioBookFileInfo> {
        if path_str.is_empty() || path::file_name_without_extension(path_str).is_empty() {
            // Return None to skip this path instead of failing the whole batch.
            return None;
        }

        let extension = path::extension(path_str);

        if !self
            .options
            .audio_file_extensions
            .iter()
            .any(|e| e.eq_ignore_ascii_case(extension))
        {
            return None;
        }

        let container = extension.trim_start_matches('.').to_string();

        let parsing_result = AudioBookFilePathParser::new(self.options).parse(path_str);

        Some(AudioBookFileInfo::new(
            path_str,
            container,
            parsing_result.part_number,
            parsing_result.chapter_number,
        ))
    }
}
