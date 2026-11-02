//! `.strm` shortcut files: the remote target a library item stands for.
//!
//! Ports `BaseVideoResolver`/`AudioResolver` (`IsShortcut` is the `.strm`
//! extension), `ProbeProvider.FetchShortcutInfo` (the first line that is not
//! blank or a `#` comment, and only when it is an `http`, `https`, `rtsp` or
//! `rtp` URL — a local path in a `.strm` would read arbitrary files off the
//! server) and `MediaSourceManager.GetPathProtocol`.

use std::path::Path;

use ferrofin_model::media_info::MediaProtocol;

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
        tracing::warn!(target = %line, "ignoring invalid or non-remote .strm path");
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

/// `MediaSourceManager.GetPathProtocol`: the protocol a path or URL is read
/// over — by its prefix, and a path that is not a local file is HTTP.
#[must_use]
pub(crate) fn path_protocol(path: &str) -> MediaProtocol {
    let lower = path.to_ascii_lowercase();
    if path.is_empty() {
        MediaProtocol::File
    } else if lower.starts_with("rtsp") {
        MediaProtocol::Rtsp
    } else if lower.starts_with("rtmp") {
        MediaProtocol::Rtmp
    } else if lower.starts_with("http") {
        MediaProtocol::Http
    } else if lower.starts_with("rtp") {
        MediaProtocol::Rtp
    } else if lower.starts_with("ftp") {
        MediaProtocol::Ftp
    } else if lower.starts_with("udp") {
        MediaProtocol::Udp
    } else if crate::media_info_resolver::is_file_protocol(path) {
        MediaProtocol::File
    } else {
        MediaProtocol::Http
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

    #[rstest]
    #[case("/media/a.mkv", MediaProtocol::File)]
    #[case("", MediaProtocol::File)]
    #[case("http://h/a", MediaProtocol::Http)]
    #[case("HTTPS://h/a", MediaProtocol::Http)]
    #[case("rtsp://h/a", MediaProtocol::Rtsp)]
    #[case("rtmp://h/a", MediaProtocol::Rtmp)]
    #[case("rtp://h/a", MediaProtocol::Rtp)]
    #[case("ftp://h/a", MediaProtocol::Ftp)]
    #[case("udp://h:1", MediaProtocol::Udp)]
    #[case("smb://h/a", MediaProtocol::Http)]
    fn the_protocol_follows_the_path_prefix(#[case] path: &str, #[case] expected: MediaProtocol) {
        assert_eq!(path_protocol(path), expected);
    }

    #[test]
    fn only_the_strm_extension_is_a_shortcut() {
        assert!(is_strm("/lib/Movie.STRM"));
        assert!(!is_strm("/lib/Movie.mkv"));
    }
}
