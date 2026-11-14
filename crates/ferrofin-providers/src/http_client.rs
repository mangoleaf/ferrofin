//! The HTTP client every provider request goes through, configured as
//! upstream's `NamedClient.Default` (`Jellyfin.Server/Startup.cs`) is for
//! AudioDb, OMDb, Studio Images, ListenBrainz, TMDb's images and
//! `ProviderManager.SaveImage`. Like it, the client names the server in its
//! `User-Agent`, decodes gzip, deflate and brotli (`AutomaticDecompression =
//! DecompressionMethods.All`), and, as .NET's `SocketsHttpHandler` does, never
//! follows a redirect from https to http (gl-76). It sends reqwest's
//! `Accept: */*`, not that client's JSON-first `Accept` list.
//!
//! It follows at most [`MAX_REDIRECTS`], reqwest's default, where .NET's
//! handler follows 50. A request that needs its own `User-Agent` (MusicBrainz,
//! LRCLIB, OpenSubtitles) sets it on the request, which replaces this one.

use crate::guarded_http::MAX_REDIRECTS;
use std::sync::OnceLock;

/// A builder with the provider defaults, for a client that needs more on top
/// (the guarded downloads' resolver and their own redirect handling).
pub(crate) fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(ferrofin_health::USER_AGENT)
        .redirect(reqwest::redirect::Policy::custom(
            |attempt| match redirect_refusal(attempt.previous(), attempt.url()) {
                Some(reason) => attempt.error(reason),
                None => attempt.follow(),
            },
        ))
}

/// The client the provider clients share, so that they share one connection
/// pool, as upstream's factory shares one handler per named client.
pub(crate) fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        // Building fails only when the TLS backend cannot start, which
        // `Client::new()`, the default, panics on just the same.
        .get_or_init(|| builder().build().unwrap_or_default())
        .clone()
}

/// Why the provider client, and a guarded download, does not follow a
/// redirect to `next`, if it does not. `previous` is reqwest's: it starts with
/// the URL first asked for, as in reqwest's own limit, and ends with the one
/// that answered this redirect.
pub(crate) fn redirect_refusal(
    previous: &[reqwest::Url],
    next: &reqwest::Url,
) -> Option<&'static str> {
    if previous.len() > MAX_REDIRECTS {
        Some("too many redirects")
    } else if previous.last().is_some_and(|from| is_downgrade(from, next)) {
        Some("a redirect from https to http")
    } else {
        None
    }
}

/// Whether a redirect from `from` to `to` leaves https for http, which
/// .NET's handler never follows.
fn is_downgrade(from: &reqwest::Url, to: &reqwest::Url) -> bool {
    from.scheme() == "https" && to.scheme() == "http"
}

#[cfg(test)]
mod tests {
    use super::{MAX_REDIRECTS, is_downgrade, redirect_refusal};
    use crate::mock_http::MockServer;
    use rstest::rstest;

    fn url(text: &str) -> reqwest::Url {
        text.parse().unwrap()
    }

    /// Only https to http is a downgrade; .NET follows every other pair.
    #[rstest]
    #[case("https://a.example/x", "http://a.example/x", true)]
    #[case("https://a.example/x", "http://b.example/y", true)]
    #[case("https://a.example/x", "https://b.example/y", false)]
    #[case("http://a.example/x", "https://a.example/x", false)]
    #[case("http://a.example/x", "http://b.example/y", false)]
    fn a_downgrade_is_https_to_http(#[case] from: &str, #[case] to: &str, #[case] expected: bool) {
        assert_eq!(is_downgrade(&url(from), &url(to)), expected);
    }

    /// The tenth redirect is followed and the eleventh is not, as with
    /// reqwest's own limit; a downgrade is refused at any hop.
    #[rstest]
    #[case(MAX_REDIRECTS, "https://a.example/next", None)]
    #[case(MAX_REDIRECTS + 1, "https://a.example/next", Some("too many redirects"))]
    #[case(1, "http://a.example/next", Some("a redirect from https to http"))]
    #[case(
        MAX_REDIRECTS,
        "http://a.example/next",
        Some("a redirect from https to http")
    )]
    fn the_policy_refuses_a_downgrade_and_the_eleventh_redirect(
        #[case] requested: usize,
        #[case] next: &str,
        #[case] expected: Option<&str>,
    ) {
        let previous = vec![url("https://a.example/x"); requested];
        assert_eq!(redirect_refusal(&previous, &url(next)), expected);
    }

    /// An http hop after an https one is the downgrade; the next http hop
    /// from there is not.
    #[test]
    fn a_downgrade_is_judged_by_the_url_that_answered() {
        let previous = [url("https://a.example/x"), url("http://a.example/y")];
        assert_eq!(
            redirect_refusal(&previous, &url("http://a.example/z")),
            None
        );
    }

    /// Through the client: a redirect loop ends after the URL asked for and
    /// ten redirects, as an error.
    #[tokio::test]
    async fn the_provider_client_stops_a_redirect_loop() {
        let server = MockServer::start_raw(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        ])
        .await;
        let error = super::client()
            .get(&server.base_url)
            .send()
            .await
            .expect_err("a loop never answers");
        assert!(error.is_redirect(), "{error:?}");
        assert_eq!(server.requests(), MAX_REDIRECTS + 1);
    }

    /// An http redirect, here to an https URL nothing answers, is followed:
    /// the policy refuses only a downgrade.
    #[tokio::test]
    async fn the_provider_client_follows_an_upgrade() {
        let server = MockServer::start_raw(vec![
            b"HTTP/1.1 302 Found\r\nLocation: https://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        ])
        .await;
        let error = super::client()
            .get(&server.base_url)
            .send()
            .await
            .expect_err("nothing answers at the target");
        assert!(error.is_connect(), "{error:?}");
    }
}
