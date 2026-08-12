//! Port of `Emby.Naming.Audio.AudioFileParser`.

use crate::common::NamingOptions;
use crate::path;

/// Determines whether the file at `path_str` is an audio file, by extension.
#[must_use]
pub fn is_audio_file(path_str: &str, options: &NamingOptions) -> bool {
    let extension = path::extension(path_str);
    options
        .audio_file_extensions
        .iter()
        .any(|e| e.eq_ignore_ascii_case(extension))
}
