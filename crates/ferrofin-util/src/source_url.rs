//! Tuner, guide and stream sources: whether one is remote, and what a log line
//! or an error may say about it.

use std::borrow::Cow;

use url::Url;

/// What [`redact_url`] shows of a URL it cannot split.
const UNREADABLE_URL: &str = "<unreadable URL>";

/// Whether `url` names a remote source rather than a local path, by upstream's
/// rule everywhere it makes that choice: it starts with `http`, ignoring ASCII
/// case (`StartsWith("http", StringComparison.OrdinalIgnoreCase)`, v12.2
/// M3uParser.cs:52, XmlTvListingsProvider.cs:76,343).
#[must_use]
pub fn is_http(url: &str) -> bool {
    url.get(..4)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http"))
}

/// What a log line or an error may say about a `source` an operator or a
/// remote server configured: a URL's scheme, host and port, and a local path as
/// it is.
///
/// Nothing past the authority is kept. Userinfo and the query are where an
/// account usually sits, but Xtream-style sources carry it in the path too
/// (`/live/<user>/<pass>/1.ts`), and such a path cannot be told from a
/// harmless one, so the id of the tuner or provider tells two sources on one
/// host apart. Local paths are fine in a log (`LOGGING.md`, rule 5): a value
/// with no scheme, or a `file:` URL. A caller holding a value it knows is a URL
/// without a scheme (an HDHomeRun tuner's `Url`) adds the scheme first.
///
/// A value is a URL when it names a scheme with `://`, or is remote by
/// [`is_http`], which `http:user:pass@host` also satisfies and `reqwest`
/// accepts. One that does not parse cannot be split safely, so nothing of it
/// is shown. That is the usual fate of an unescaped `/`, `?` or `#` in a
/// password, which ends the authority early; one that starts with digits still
/// parses, as a port, and those digits show with the user name as the host.
#[must_use]
pub fn redact_url(source: &str) -> String {
    if !is_http(source) && !source.contains("://") {
        return source.to_owned();
    }
    let Ok(url) = Url::parse(source) else {
        return UNREADABLE_URL.to_owned();
    };
    if url.scheme() == "file" {
        return source.to_owned();
    }
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

/// Whether `value`, as a whole, is a URL naming something remote: it starts
/// with `<scheme>://`, a `file` URL aside, or with `http:` or `https:`, which
/// ffmpeg and [`redact_url`] both read without the slashes.
#[must_use]
pub fn is_remote_url(value: &str) -> bool {
    value
        .find(':')
        .is_some_and(|colon| scheme_start(value, 0, colon) == Some(0) && names_url(value, 0, colon))
}

/// `text` with each URL in it passed through [`redact_url`], for text Ferrofin
/// did not write, such as a line of ffmpeg's stderr. Borrowed when `text` holds
/// no URL.
///
/// A URL right after a quote runs to the last such quote in `text`, or to the
/// end when there is none: ffmpeg prints a URL unescaped, so a password can
/// hold anything. Any other URL runs to the next white space, less the
/// punctuation that closes a sentence around it.
#[must_use]
pub fn redact_urls_in(text: &str) -> Cow<'_, str> {
    let mut redacted = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(start) = next_url(text, cursor) {
        let end = url_end(text, start);
        let url = text[start..end].trim_end_matches(['.', ',', ';', ':', ')']);
        redacted.push_str(&text[copied..start]);
        redacted.push_str(&redact_url(url));
        copied = start + url.len();
        cursor = end;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    redacted.push_str(&text[copied..]);
    Cow::Owned(redacted)
}

/// Where the first URL at or after `from` in `text` starts.
fn next_url(text: &str, from: usize) -> Option<usize> {
    let mut search = from;
    while let Some(found) = text[search..].find(':') {
        let colon = search + found;
        if let Some(start) = scheme_start(text, from, colon)
            && names_url(text, start, colon)
        {
            return Some(start);
        }
        search = colon + 1;
    }
    None
}

/// Where the scheme ending at `colon` starts, no earlier than `from`.
fn scheme_start(text: &str, from: usize, colon: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let run = bytes[from..colon]
        .iter()
        .rposition(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')))
        .map_or(from, |before| from + before + 1);
    (run..colon).find(|&at| bytes[at].is_ascii_alphabetic())
}

/// Whether the scheme `text[start..colon]` begins a remote URL.
fn names_url(text: &str, start: usize, colon: usize) -> bool {
    let scheme = &text[start..colon];
    let rest = &text[colon + 1..];
    if rest.starts_with("//") {
        return !scheme.eq_ignore_ascii_case("file");
    }
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && rest
            .chars()
            .next()
            .is_some_and(|next| !next.is_whitespace() && !matches!(next, '\'' | '"'))
}

/// Where the URL starting at `start` in `text` ends; see [`redact_urls_in`].
fn url_end(text: &str, start: usize) -> usize {
    match text[..start].chars().next_back() {
        Some(quote @ ('\'' | '"')) => text
            .rfind(quote)
            .filter(|&end| end >= start)
            .unwrap_or(text.len()),
        _ => text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |end| start + end),
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use rstest::rstest;

    use super::{is_http, is_remote_url, redact_url, redact_urls_in};

    /// `StartsWith("http", StringComparison.OrdinalIgnoreCase)`: the scheme in
    /// any case is remote; a path, `file://` and anything shorter are not.
    #[rstest]
    #[case("http://guide/x.xml", true)]
    #[case("HTTP://guide/x.xml", true)]
    #[case("Http://guide/x.xml", true)]
    #[case("https://guide/x.xml", true)]
    #[case("/srv/guide.xml", false)]
    #[case("file:///srv/guide.xml", false)]
    #[case("C:\\guide.xml", false)]
    #[case("htt", false)]
    #[case("", false)]
    #[case("htté://x", false)]
    fn a_source_is_remote_when_it_starts_with_http(#[case] url: &str, #[case] remote: bool) {
        assert_eq!(is_http(url), remote, "{url}");
    }

    /// A URL keeps its scheme, host and port and nothing else, wherever the
    /// account sits; a local path stays, and what cannot be split shows nothing.
    #[rstest]
    #[case::bare("http://tuner.example", "http://tuner.example")]
    #[case::a_port("http://tuner.example:8080/list.m3u", "http://tuner.example:8080")]
    #[case::the_default_port("http://tuner.example:80/x", "http://tuner.example")]
    #[case::userinfo("http://user:secret@tuner.example/", "http://tuner.example")]
    #[case::xtream_query(
        "http://tuner.example/get.php?username=user&password=secret&type=m3u_plus",
        "http://tuner.example"
    )]
    #[case::xtream_path("http://tuner.example/live/user/secret/1.ts", "http://tuner.example")]
    #[case::fragment("https://tuner.example/#secret", "https://tuner.example")]
    #[case::an_at_in_the_password("http://user:s@ecret@tuner.example/", "http://tuner.example")]
    #[case::upper_case_scheme("HTTP://user:secret@tuner.example/", "http://tuner.example")]
    #[case::no_slashes("http:user:secret@tuner.example/x", "http://tuner.example")]
    #[case::ipv6("http://user:secret@[::1]:8080/x", "http://[::1]:8080")]
    #[case::rtsp("rtsp://user:secret@cam.example:554/s", "rtsp://cam.example:554")]
    #[case::multicast("udp://@239.0.0.1:1234", "udp://239.0.0.1:1234")]
    #[case::path("/srv/guide.xml", "/srv/guide.xml")]
    #[case::relative_path("guides/guide.xml", "guides/guide.xml")]
    #[case::windows_path("C:\\guide.xml", "C:\\guide.xml")]
    #[case::file_url("file:///srv/guide.xml", "file:///srv/guide.xml")]
    #[case::empty("", "")]
    #[case::a_slash_in_the_password(
        "http://user:se/cret@tuner.example/",
        crate::source_url::UNREADABLE_URL
    )]
    #[case::a_hash_in_the_password(
        "http://user:se#cret@tuner.example/",
        crate::source_url::UNREADABLE_URL
    )]
    #[case::remote_by_upstreams_rule("http-not-a-url/secret", crate::source_url::UNREADABLE_URL)]
    #[case::no_host("http://", crate::source_url::UNREADABLE_URL)]
    fn a_source_is_shown_by_its_scheme_and_host(#[case] source: &str, #[case] shown: &str) {
        assert_eq!(redact_url(source), shown, "{source}");
    }

    /// A value is a remote URL as a whole, by its scheme; what only holds a
    /// URL, an ffmpeg option and a local path or `file` URL are not.
    #[rstest]
    #[case("http://tuner.example/1.m3u8", true)]
    #[case("HTTPS://tuner.example", true)]
    #[case("http:user:secret@tuner.example/1.m3u8", true)]
    #[case("rtsp://cam.example/s", true)]
    #[case("udp://@239.0.0.1:1234", true)]
    #[case("crypto+http://tuner.example/s.ts", true)]
    #[case("file:/media/a.mkv", false)]
    #[case("file:///media/a.mkv", false)]
    #[case("FILE:///media/a.mkv", false)]
    #[case("/media/http://a.mkv", false)]
    #[case("title=http://a.example/", false)]
    #[case("http,https,tcp", false)]
    #[case("http", false)]
    #[case("http:", false)]
    #[case("pipe:0", false)]
    #[case("0://x", false)]
    #[case("", false)]
    fn a_remote_url_is_told_by_its_scheme(#[case] value: &str, #[case] remote: bool) {
        assert_eq!(is_remote_url(value), remote, "{value}");
    }

    /// Every URL in a line goes, however ffmpeg framed it, and what surrounds
    /// it stays; a line with none comes back as it is.
    #[rstest]
    #[case::bare_sentence(
        "Error opening input file http://u:secret@tuner.example/1.m3u8.",
        "Error opening input file http://tuner.example."
    )]
    #[case::single_quotes(
        "Input #0, hls, from 'http://u:secret@tuner.example:81/a?k=secret':",
        "Input #0, hls, from 'http://tuner.example:81':"
    )]
    #[case::double_quotes(
        "got \"https://tuner.example/secret\" now",
        "got \"https://tuner.example\" now"
    )]
    #[case::a_space_inside_quotes(
        "Opening 'http://tuner.example/my secret/1.ts' for reading",
        "Opening 'http://tuner.example' for reading"
    )]
    #[case::a_quote_in_the_password(
        "Opening 'http://u:se'cret@tuner.example/1.ts' for reading",
        "Opening 'http://tuner.example' for reading"
    )]
    #[case::no_closing_quote(
        "Opening 'http://tuner.example/secret and more secret",
        "Opening 'http://tuner.example"
    )]
    #[case::two_urls(
        "from http://a.example/secret to https://b.example/secret",
        "from http://a.example to https://b.example"
    )]
    #[case::an_inner_scheme(
        "crypto+http://u:secret@a.example/k and hls+https://b.example/secret",
        "crypto+http://a.example and hls+https://b.example"
    )]
    #[case::a_prefixed_protocol("crypto:http://a.example/secret", "crypto:http://a.example")]
    #[case::no_slashes(
        "file http:user:secret@tuner.example/x failed",
        "file http://tuner.example failed"
    )]
    #[case::port_only(
        "Connection to tcp://127.0.0.1:9 failed: Connection refused",
        "Connection to tcp://127.0.0.1:9 failed: Connection refused"
    )]
    #[case::multibyte(
        "caf\u{e9}: http://a.example/s\u{e9}cret \u{e9}",
        "caf\u{e9}: http://a.example \u{e9}"
    )]
    #[case::a_file_url(
        "Opening 'file:///tmp/a.ts' for reading",
        "Opening 'file:///tmp/a.ts' for reading"
    )]
    fn urls_in_a_line_are_redacted(#[case] text: &str, #[case] shown: &str) {
        assert_eq!(redact_urls_in(text), shown, "{text}");
    }

    /// ffmpeg's ordinary lines hold plenty of colons and no URL.
    #[rstest]
    #[case("Stream #0:0: Video: h264 (High), yuv420p, 1920x1080")]
    #[case("  Duration: 00:01:40.00, start: 1.400000, bitrate: N/A")]
    #[case("frame=  100 fps=25 q=-1.0 time=00:00:04.00 speed=1.0x")]
    #[case("Protocol 'http' not on whitelist 'file,crypto,data'!")]
    #[case("http: the connection broke")]
    #[case("")]
    fn a_line_without_a_url_is_left_alone(#[case] text: &str) {
        assert!(matches!(redact_urls_in(text), Cow::Borrowed(_)), "{text}");
    }
}
