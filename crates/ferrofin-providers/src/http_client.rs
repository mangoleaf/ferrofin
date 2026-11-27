//! The HTTP client every provider request goes through, configured as
//! upstream's `NamedClient.Default` (`Jellyfin.Server/Startup.cs`) is for
//! AudioDb, OMDb, Studio Images, ListenBrainz, TMDb's images and
//! `ProviderManager.SaveImage`: [`ferrofin_util::http_client::builder`], which
//! names the server in its `User-Agent`, sends that client's JSON-first
//! `Accept` list, decodes gzip, deflate and brotli, and never follows a
//! redirect from https to http (gl-76).
//!
//! Unlike that builder's, a redirect it does not follow is an error, not the
//! 3xx: the rate limiter counts on this client never handing one back
//! (`rate_limit::is_redirect_to_follow`), and a provider reads a 3xx as a
//! failure either way. A request that needs its own `User-Agent` (MusicBrainz,
//! LRCLIB, OpenSubtitles) or `Accept` (OpenSubtitles) sets it on the request,
//! which replaces this one.

use ferrofin_util::http_client::redirect_refusal;
use std::sync::OnceLock;

/// A builder with the provider defaults, for a client that needs more on top
/// (the guarded downloads' resolver and their own redirect handling).
pub(crate) fn builder() -> reqwest::ClientBuilder {
    ferrofin_util::http_client::builder(ferrofin_health::USER_AGENT).redirect(
        reqwest::redirect::Policy::custom(|attempt| {
            match redirect_refusal(attempt.previous(), attempt.url()) {
                Some(reason) => attempt.error(reason),
                None => attempt.follow(),
            }
        }),
    )
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

#[cfg(test)]
mod tests {
    use crate::mock_http::MockServer;
    use ferrofin_util::http_client::MAX_REDIRECTS;

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

    /// A provider request has `NamedClient.Default`'s `User-Agent` and
    /// `Accept` list; a request's own `Accept` (OpenSubtitles') replaces the
    /// list, as `HttpHeaders.AddHeaders` keeps a request's own header.
    #[tokio::test]
    async fn the_provider_client_sends_the_default_client_headers() {
        let server = MockServer::start_raw(vec![MockServer::raw_ok("", b"{}")]).await;
        super::client().get(&server.base_url).send().await.unwrap();
        super::client()
            .get(&server.base_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .unwrap();
        let heads = server.heads();
        let agent = format!(
            "\r\nuser-agent: {}\r\n",
            ferrofin_health::USER_AGENT.to_ascii_lowercase()
        );
        assert!(heads[0].contains(&agent), "{}", heads[0]);
        assert!(
            heads[0].contains(
                "\r\naccept: application/json; q=1.0, application/xml; q=0.9, */*; q=0.8\r\n"
            ),
            "{}",
            heads[0]
        );
        assert!(
            heads[1].contains("\r\naccept: application/json\r\n"),
            "{}",
            heads[1]
        );
        assert_eq!(heads[1].matches("\r\naccept:").count(), 1, "{}", heads[1]);
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
