//! `.strm` shortcut files: the remote target a library item stands for.
//!
//! Ports `BaseVideoResolver`/`AudioResolver` (`IsShortcut` is the `.strm`
//! extension), `ProbeProvider.FetchShortcutInfo` (the first line that is not
//! blank or a `#` comment, and only when it is an `http`, `https`, `rtsp` or
//! `rtp` URL — a local path in a `.strm` would read arbitrary files off the
//! server). The target's protocol is
//! [`path_protocol`](ferrofin_model::media_info::path_protocol).

use std::path::Path;

/// The schemes a `.strm` target may use (`ProbeProvider.FetchShortcutInfo`).
const REMOTE_SCHEMES: [&str; 4] = ["http", "https", "rtsp", "rtp"];

/// `IsShortcut`: the item's file is a `.strm`.
#[must_use]
pub(crate) fn is_strm(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("strm"))
}

/// `ProbeProvider.NormalizeStrmLine`: tabs and line breaks removed, then trimmed.
fn normalize_line(line: &str) -> String {
    line.replace(['\t', '\r', '\n'], "").trim().to_owned()
}

/// The target a `.strm`'s text names: its first line that is not blank or a
/// `#` comment, when that line is an absolute remote URL. `None` when the file
/// has no such line (the item keeps what it had) or the line is not remote
/// (`Ignoring invalid or non-remote .strm path`).
#[must_use]
pub(crate) fn remote_target(contents: &str) -> Option<String> {
    let line = contents
        .split(['\r', '\n'])
        .map(normalize_line)
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'))?;
    let remote = reqwest::Url::parse(&line)
        .is_ok_and(|url| REMOTE_SCHEMES.contains(&url.scheme().to_ascii_lowercase().as_str()));
    if !remote {
        // A URL the line names can carry an account, like any media URL.
        tracing::warn!(
            target = %ferrofin_util::source_url::redact_url(&line),
            "ignoring invalid or non-remote .strm path"
        );
        return None;
    }
    Some(line)
}

/// Reads a `.strm` file's remote target ([`remote_target`]); an unreadable
/// file names nothing.
pub(crate) async fn read_target(path: &str) -> Option<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => remote_target(&contents),
        Err(error) => {
            tracing::warn!(%error, path, "cannot read .strm file");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("http://host/a.mkv", Some("http://host/a.mkv"))]
    #[case(
        "# comment\n\n  https://host/b.mp4\t\r\nignored",
        Some("https://host/b.mp4")
    )]
    #[case("RTSP://cam/stream", Some("RTSP://cam/stream"))]
    #[case("rtp://239.0.0.1:5000", Some("rtp://239.0.0.1:5000"))]
    #[case("/etc/passwd", None)]
    #[case("file:///etc/passwd", None)]
    #[case("ftp://host/a.mkv", None)]
    #[case("# only a comment\n   \n", None)]
    // The first usable line decides: a later URL is never reached.
    #[case("/etc/passwd\nhttp://host/a.mkv", None)]
    fn the_first_usable_line_must_be_a_remote_url(
        #[case] contents: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(remote_target(contents).as_deref(), expected);
    }

    /// A target refused for its scheme is logged by its scheme and host: it can
    /// carry an account like any URL (gl-1121).
    #[test]
    fn a_refused_target_is_logged_by_its_host() {
        let logs = crate::test_support::CapturedLogs::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        );
        let line =
            "ftp://acct-user:info-secret@origin.example/path-secret/a.mkv?password=query-secret";
        assert_eq!(remote_target(line), None);

        let text = logs.text();
        assert!(text.contains("ftp://origin.example"), "{text}");
        for secret in ["acct-user", "info-secret", "path-secret", "query-secret"] {
            assert!(!text.contains(secret), "{secret} logged: {text}");
        }
    }

    #[test]
    fn only_the_strm_extension_is_a_shortcut() {
        assert!(is_strm("/lib/Movie.STRM"));
        assert!(!is_strm("/lib/Movie.mkv"));
    }
}
