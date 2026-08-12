//! Port of `MediaBrowser.MediaEncoding.Encoder.EncodingUtils`.
//!
//! Pure string helpers that build the `-i` input argument for an ffmpeg
//! command line, handling non-file protocols, the `concat:` form for
//! multi-part inputs, `://` URLs, and quote escaping. No I/O.

use ferrofin_model::media_info::MediaProtocol;

/// Gets the ffmpeg input argument for a single input file.
///
/// For non-`File` protocols the path is simply wrapped in double quotes; for
/// `File` it is routed through [`get_file_input_argument`] (which applies the
/// `inputPrefix:` scheme and quote escaping).
#[must_use]
pub fn get_input_argument(input_prefix: &str, input_file: &str, protocol: MediaProtocol) -> String {
    if protocol != MediaProtocol::File {
        return format!("\"{input_file}\"");
    }

    get_file_input_argument(input_file, input_prefix)
}

/// Gets the ffmpeg input argument for a list of input files.
///
/// For non-`File` protocols the first file is wrapped in double quotes; for
/// `File` the list is routed through [`get_concat_input_argument`].
///
/// # Panics
///
/// Panics if `input_files` is empty, matching the C# indexing behaviour.
#[must_use]
pub fn get_input_argument_multi(
    input_prefix: &str,
    input_files: &[String],
    protocol: MediaProtocol,
) -> String {
    if protocol != MediaProtocol::File {
        return format!("\"{}\"", input_files[0]);
    }

    get_concat_input_argument(input_files, input_prefix)
}

/// Gets the concat input argument for one or more input files.
///
/// With more than one file, produces `concat:"a|b|c"` (each path normalized);
/// otherwise falls back to the single-file form.
fn get_concat_input_argument(input_files: &[String], input_prefix: &str) -> String {
    // Get all streams
    // If there's more than one we'll need to use the concat command
    if input_files.len() > 1 {
        let files = input_files
            .iter()
            .map(|p| normalize_path(p))
            .collect::<Vec<_>>()
            .join("|");

        return format!("concat:\"{files}\"");
    }

    // Determine the input path for video files
    get_file_input_argument(&input_files[0], input_prefix)
}

/// Gets the file input argument.
///
/// A `://` URL is quoted verbatim; a plain path is normalized (escaping any
/// embedded quotes) and prefixed with `inputPrefix:`.
fn get_file_input_argument(path: &str, input_prefix: &str) -> String {
    if path.contains("://") {
        return format!("\"{path}\"");
    }

    // Quotes are valid path characters in linux and they need to be escaped here with a leading \
    let path = normalize_path(path);

    format!("{input_prefix}:\"{path}\"")
}

/// Normalizes a path by escaping embedded double quotes with a leading `\`.
///
/// Quotes are valid path characters on Linux, so they must be escaped for the
/// ffmpeg command line.
#[must_use]
pub fn normalize_path(path: &str) -> String {
    // Quotes are valid path characters in linux and they need to be escaped here with a leading \
    path.replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_file_protocol_wraps_in_quotes() {
        assert_eq!(
            get_input_argument("file", "http://host/a.mkv", MediaProtocol::Http),
            "\"http://host/a.mkv\""
        );
    }

    #[test]
    fn file_protocol_applies_prefix() {
        assert_eq!(
            get_input_argument("file", "/media/a.mkv", MediaProtocol::File),
            "file:\"/media/a.mkv\""
        );
    }

    #[test]
    fn url_scheme_is_quoted_without_prefix() {
        assert_eq!(
            get_input_argument("file", "https://host/a.mkv", MediaProtocol::File),
            "\"https://host/a.mkv\""
        );
    }

    #[test]
    fn embedded_quote_is_escaped() {
        assert_eq!(normalize_path("/media/a\"b.mkv"), "/media/a\\\"b.mkv");
        assert_eq!(
            get_input_argument("file", "/media/a\"b.mkv", MediaProtocol::File),
            "file:\"/media/a\\\"b.mkv\""
        );
    }

    #[test]
    fn single_file_list_uses_file_form() {
        let files = vec!["/media/a.mkv".to_owned()];
        assert_eq!(
            get_input_argument_multi("file", &files, MediaProtocol::File),
            "file:\"/media/a.mkv\""
        );
    }

    #[test]
    fn multi_file_list_uses_concat_form() {
        let files = vec!["/media/a.mkv".to_owned(), "/media/b.mkv".to_owned()];
        assert_eq!(
            get_input_argument_multi("file", &files, MediaProtocol::File),
            "concat:\"/media/a.mkv|/media/b.mkv\""
        );
    }

    #[test]
    fn multi_file_list_non_file_uses_first() {
        let files = vec!["a.mkv".to_owned(), "b.mkv".to_owned()];
        assert_eq!(
            get_input_argument_multi("file", &files, MediaProtocol::Http),
            "\"a.mkv\""
        );
    }
}
