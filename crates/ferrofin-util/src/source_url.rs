//! Tuner, guide and stream sources: whether one is remote, and what a log line
//! or an error may say about it.

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

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::{is_http, redact_url};

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
}
