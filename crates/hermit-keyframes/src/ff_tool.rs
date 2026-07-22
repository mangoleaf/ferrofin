//! `FfTool` based keyframe extractor.
//!
//! Port of `Jellyfin.MediaEncoding.Keyframes.FfTool.FfToolKeyframeExtractor`.
//! The upstream C# body throws `NotImplementedException`; this is reproduced as
//! an unimplemented stub to preserve the API surface.

use crate::keyframe_data::KeyframeData;

/// Extracts the keyframes using the fftool executable at the specified path.
///
/// # Arguments
///
/// * `ff_tool_path` - The path to the fftool executable.
/// * `file_path` - The file path.
///
/// # Panics
///
/// Always panics: this mirrors the upstream C# `throw new NotImplementedException()`.
#[must_use]
pub fn get_keyframe_data(ff_tool_path: &str, file_path: &str) -> KeyframeData {
    let _ = (ff_tool_path, file_path);
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "not implemented")]
    fn get_keyframe_data_is_unimplemented() {
        // Mirrors upstream C# `throw new NotImplementedException()`.
        let _ = get_keyframe_data("/path/to/fftool", "/path/to/file.mkv");
    }
}
