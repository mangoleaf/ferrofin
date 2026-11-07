//! Port of `Emby.Naming.Audio.AlbumParser`.

use std::sync::OnceLock;

use ferrofin_util::string_extensions::left_part;
use regex::Regex;

use crate::common::NamingOptions;
use crate::path;

/// Upstream's `[GeneratedRegex(@"[-\.\(\)\s]+")] CleanRegex()`, compiled once per process.
fn clean_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[-\.\(\)\s]+").expect("AlbumParser clean regex is valid"))
}

/// Helper to determine whether an album directory is multipart.
pub struct AlbumParser<'a> {
    options: &'a NamingOptions,
}

impl<'a> AlbumParser<'a> {
    /// Creates a new [`AlbumParser`] bound to the given options.
    #[must_use]
    pub fn new(options: &'a NamingOptions) -> Self {
        Self { options }
    }

    /// Determines whether the album at `path_str` is multipart.
    #[must_use]
    pub fn is_multi_part(&self, path_str: &str) -> bool {
        let filename = path::file_name(path_str);
        if filename.is_empty() {
            return false;
        }

        // Normalize: collapse whitespace/punctuation runs to a single space.
        let filename = clean_regex().replace_all(filename, " ");
        let trimmed_filename = filename.trim_start();

        for prefix in &self.options.album_stacking_prefixes {
            let Some(head) = trimmed_filename.get(..prefix.len()) else {
                continue;
            };
            if !head.eq_ignore_ascii_case(prefix) {
                continue;
            }

            let tmp = trimmed_filename[prefix.len()..].trim();

            if left_part(tmp, ' ').parse::<i64>().is_ok() {
                return true;
            }
        }

        false
    }
}
