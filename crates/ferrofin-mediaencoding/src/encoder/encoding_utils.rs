//! Port of `MediaBrowser.MediaEncoding.Encoder.EncodingUtils`.
//!
//! Pure string helpers that build the `-i` input argument for an ffmpeg
//! command line, handling non-file protocols, the `concat:` form for
//! multi-part inputs, `://` URLs, and quote escaping. No I/O.
//!
//! Every ffmpeg command built as one string (as upstream builds them) is split
//! back into argv by [`split_args`] before spawning. [`quote`] is that split's exact
//! inverse, so any value a builder interpolates (a media path, a URL, a header)
//! comes back as one argv token and can never add an ffmpeg option.
//!
//! Upstream escapes only `File` paths and leaves non-`File` inputs, `://` URLs
//! and the tuner `-user_agent` value raw inside their quotes. Ferrofin quotes
//! all of them: a remote source or header is no more trusted than a file name.

use std::borrow::Cow;

use ferrofin_model::media_info::MediaProtocol;

/// Gets the ffmpeg input argument for a single input file.
///
/// For non-`File` protocols the path is simply [`quote`]d; for `File` it is
/// routed through [`get_file_input_argument`] (which applies the
/// `inputPrefix:` scheme and quote escaping).
#[must_use]
pub fn get_input_argument(input_prefix: &str, input_file: &str, protocol: MediaProtocol) -> String {
    if protocol != MediaProtocol::File {
        return quote(input_file);
    }

    get_file_input_argument(input_file, input_prefix)
}

/// Gets the ffmpeg input argument for a list of input files.
///
/// For non-`File` protocols the first file is [`quote`]d; for `File` the list
/// is routed through [`get_concat_input_argument`].
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
        return quote(&input_files[0]);
    }

    get_concat_input_argument(input_files, input_prefix)
}

/// Gets the concat input argument for one or more input files.
///
/// With more than one file, produces `concat:"a|b|c"` (each path escaped);
/// otherwise falls back to the single-file form.
fn get_concat_input_argument(input_files: &[String], input_prefix: &str) -> String {
    // Get all streams
    // If there's more than one we'll need to use the concat command
    if input_files.len() > 1 {
        let files = input_files
            .iter()
            .map(|p| escape_process_argument(p))
            .collect::<Vec<_>>()
            .join("|");

        return format!("concat:\"{files}\"");
    }

    // Determine the input path for video files
    get_file_input_argument(&input_files[0], input_prefix)
}

/// Gets the file input argument.
///
/// A `://` URL is [`quote`]d without a prefix; a plain path is quoted and
/// prefixed with `inputPrefix:`.
fn get_file_input_argument(path: &str, input_prefix: &str) -> String {
    if path.contains("://") {
        return quote(path);
    }

    format!("{input_prefix}:{}", quote(path))
}

/// Escapes `value` so it survives [`split_args`] as a single argument when the
/// caller wraps it in double quotes.
///
/// Port of `StringExtensions.EscapeProcessArgument` (which replaced
/// `EncodingUtils.NormalizePath` in 12.0). Upstream escapes for .NET's argv
/// rules, where a backslash is literal unless a `"` follows it, so it doubles
/// only the backslashes before a quote or at the end. [`split_args`] follows
/// POSIX shell rules instead, where `\\` is always an escape inside double
/// quotes, so every `\` is doubled and every `"` escaped.
#[must_use]
pub fn escape_process_argument(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Wraps `value` in double quotes for an ffmpeg argument string, escaped so
/// [`split_args`] returns exactly `value` as a single argument.
#[must_use]
pub fn quote(value: &str) -> String {
    format!("\"{}\"", escape_process_argument(value))
}

/// Quotes `value` for [`split_args`] unless it is made only of ASCII letters,
/// digits and `+ - . / : @ ] _` (shlex 2's unquoted set; non-ASCII is quoted
/// too), using shlex's POSIX quoting (or [`quote`] for a value with a NUL byte,
/// which shlex refuses). A plain value such as
/// `/dev/dri/renderD128` therefore stays byte-identical to upstream's command
/// line, while one with a space, quote or backslash still splits back whole.
#[must_use]
pub fn quote_if_needed(value: &str) -> Cow<'_, str> {
    shlex::try_quote(value).unwrap_or_else(|_| quote(value).into())
}

/// Splits an ffmpeg argument string into argv, honouring the quoting [`quote`]
/// emits: `-i file:"/path/with spaces/f.mkv"` reaches ffmpeg as one `-i`
/// value, not two.
///
/// # Errors
///
/// Returns an error naming the string when its quoting is unbalanced. That is
/// a builder bug, and guessing at the split could hand part of a path to
/// ffmpeg as options, so the caller must not guess: it refuses the command,
/// or passes the whole string as one argument, which cannot add an option.
pub fn split_args(arguments: &str) -> Result<Vec<String>, String> {
    shlex::split(arguments)
        .ok_or_else(|| format!("unbalanced quoting in ffmpeg arguments `{arguments}`"))
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

    #[rstest::rstest]
    #[case::plain("/media/a.mkv")]
    #[case::spaces("/media/My  Movie (2020).mkv")]
    #[case::quote(r#"/media/a"b.mkv"#)]
    #[case::backslash_quote_injection(r#"/lib/a\" -y -i /etc/passwd \".mp3"#)]
    #[case::trailing_backslash(r"/media/a\")]
    #[case::windows(r"C:\Media\a.mkv")]
    #[case::shell_chars("/m/$HOME `id` 'x' #y\nz.mkv")]
    #[case::shlex_escapes("/m/a\\\\b\\$c\\`d\\\ne.mkv")]
    // The inputs of upstream's `EscapeProcessArgument_ValidInput_Corrects`
    // (their escaped forms are pinned in `escape_matches_upstream`).
    #[case::upstream_empty("")]
    #[case::upstream_plain("/media/movies/Film.mkv")]
    #[case::upstream_windows(r"C:\media\movies\Film.mkv")]
    #[case::upstream_quote(r#"/media/a"b.mkv"#)]
    #[case::upstream_backslash_quote(r#"/media/a\"b.mkv"#)]
    #[case::upstream_two_backslashes_quote(r#"/media/a\\"b.mkv"#)]
    #[case::upstream_backslash_then_quote(r#"/media/a\b"c.mkv"#)]
    #[case::upstream_trailing(r"/media/trailing\")]
    #[case::upstream_evil(r#"/media/evil\" -f lavfi -i sine .mkv"#)]
    fn quote_round_trips_through_split(#[case] value: &str) {
        let args = format!("-y -i file:{} -f null -", quote(value));
        assert_eq!(
            split_args(&args).unwrap(),
            ["-y", "-i", &format!("file:{value}"), "-f", "null", "-"]
        );
        let url = format!("http://h/{value}");
        assert_eq!(
            split_args(&get_input_argument("file", &url, MediaProtocol::Http)).unwrap(),
            [url]
        );
    }

    #[rstest::rstest]
    #[case::space("/dev/dri/my gpu")]
    #[case::single_quote("/dev/dri/a'b")]
    #[case::double_quote(r#"/dev/dri/a"b"#)]
    #[case::backslash(r"/dev/dri/a\b")]
    #[case::shell_chars("/dev/$HOME #x")]
    #[case::non_ascii("/dev/dri/gpu-é")]
    #[case::nul("/dev/dri/a\0b")]
    fn quote_if_needed_round_trips_through_split(#[case] value: &str) {
        let args = format!("-init_hw_device drm=dr:{}", quote_if_needed(value));
        assert_eq!(
            split_args(&args).unwrap(),
            ["-init_hw_device", &format!("drm=dr:{value}")]
        );
    }

    // The golden-string guarantee: an ordinary device path is not rewritten.
    #[test]
    fn quote_if_needed_leaves_a_plain_device_path_untouched() {
        for path in [
            "/dev/dri/renderD128",
            "/dev/dri/by-path/pci-0000:03:00.0-render",
        ] {
            assert!(matches!(quote_if_needed(path), Cow::Borrowed(p) if p == path));
        }
    }

    /// Transliteration of upstream's `EscapeProcessArgument_ValidInput_Corrects`
    /// and `EscapeProcessArgument_NothingToEscape_ReturnsSameInstance`. Every
    /// expected value is upstream's own except the two `diverges_*` cases,
    /// whose backslash is neither before a quote nor at the end. The rule
    /// differs there, not the round trip: upstream leaves such a backslash
    /// alone, which .NET's parser keeps, while [`split_args`] also treats
    /// `\\`, `\$` and `` \` `` as escapes, so every backslash is doubled
    /// (`shlex_escapes` in the round-trip cases pins why).
    #[rstest::rstest]
    #[case::empty("", "")]
    #[case::plain("/media/movies/Film.mkv", "/media/movies/Film.mkv")]
    #[case::diverges_windows(r"C:\media\movies\Film.mkv", r"C:\\media\\movies\\Film.mkv")]
    #[case::quote(r#"/media/a"b.mkv"#, r#"/media/a\"b.mkv"#)]
    #[case::backslash_quote(r#"/media/a\"b.mkv"#, r#"/media/a\\\"b.mkv"#)]
    #[case::two_backslashes_quote(r#"/media/a\\"b.mkv"#, r#"/media/a\\\\\"b.mkv"#)]
    #[case::diverges_backslash_then_quote(r#"/media/a\b"c.mkv"#, r#"/media/a\\b\"c.mkv"#)]
    #[case::trailing(r"/media/trailing\", r"/media/trailing\\")]
    #[case::evil(
        r#"/media/evil\" -f lavfi -i sine .mkv"#,
        r#"/media/evil\\\" -f lavfi -i sine .mkv"#
    )]
    #[case::nothing_to_escape(
        "/media/movies/Film with spaces.mkv",
        "/media/movies/Film with spaces.mkv"
    )]
    fn escape_matches_upstream(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(escape_process_argument(input), expected);
    }

    #[test]
    fn unbalanced_quoting_fails_closed() {
        let err = split_args(r#"-i file:"/media/a.mkv -y out"#).unwrap_err();
        assert!(err.contains("unbalanced quoting"), "{err}");
    }

    #[test]
    fn embedded_backslash_is_escaped() {
        assert_eq!(
            escape_process_argument(r#"/media/a\"b"#),
            r#"/media/a\\\"b"#
        );
    }

    #[test]
    fn embedded_quote_is_escaped() {
        assert_eq!(
            escape_process_argument("/media/a\"b.mkv"),
            "/media/a\\\"b.mkv"
        );
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
