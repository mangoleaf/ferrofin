//! Defaults of .NET's `System.Net.Http.HttpClient` that Jellyfin's outbound
//! requests inherit, for the Ferrofin clients that port those requests.
//!
//! Upstream builds them from `NamedClient.Default` (`Jellyfin.Server/Startup.cs`,
//! `ConfigureServices`): the product `User-Agent`, the [`DEFAULT_ACCEPT`] list,
//! `AutomaticDecompression = All` (reqwest's `gzip`, `brotli` and `deflate`
//! features, on workspace-wide) and `SocketsHttpHandler`'s redirect handling,
//! which never follows a redirect from https to http. With the `reqwest`
//! feature, [`builder`] makes a client with those defaults.

use std::time::Duration;

/// .NET's default `HttpClient.Timeout`: 100 seconds.
///
/// Jellyfin's `NamedClient.Default` (`Jellyfin.Server/Startup.cs`) never sets
/// it, and neither does the File Transformation plugin's `new HttpClient()`
/// (`TransformationHelper.ApplyTransformation`), so the requests they send give
/// up after 100 s. A request sent with `HttpCompletionOption.ResponseHeadersRead`
/// is bounded up to its response headers and its body is read without a limit;
/// any other request is bounded in full, body included.
pub const HTTP_CLIENT_DEFAULT_TIMEOUT: Duration = Duration::from_secs(100);

/// `NamedClient.Default`'s `Accept` header: JSON at quality 1.0, XML at 0.9
/// and anything else at 0.8, as .NET writes the three
/// `MediaTypeWithQualityHeaderValue`s (captured from Jellyfin 12.4).
pub const DEFAULT_ACCEPT: &str = "application/json; q=1.0, application/xml; q=0.9, */*; q=0.8";

/// Most redirects one request follows: reqwest's default policy
/// (`redirect::Policy::default()`), where .NET's handler follows 50.
pub const MAX_REDIRECTS: usize = 10;

/// Why a client does not follow a redirect to `next`, if it does not.
/// `previous` is reqwest's: it starts with the URL first asked for, as in
/// reqwest's own limit, and ends with the one that answered this redirect.
#[must_use]
pub fn redirect_refusal(previous: &[url::Url], next: &url::Url) -> Option<&'static str> {
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
fn is_downgrade(from: &url::Url, to: &url::Url) -> bool {
    from.scheme() == "https" && to.scheme() == "http"
}

/// A client builder with `NamedClient.Default`'s defaults, naming the server
/// as `user_agent`.
///
/// A request's own `User-Agent` or `Accept` replaces the default, as
/// `HttpHeaders.AddHeaders` keeps a request's own header. A redirect
/// [`redirect_refusal`] refuses is not followed: the 3xx is the response, as
/// `SocketsHttpHandler` returns it.
#[cfg(feature = "reqwest")]
pub fn builder(user_agent: &str) -> reqwest::ClientBuilder {
    let accept = reqwest::header::HeaderValue::from_static(DEFAULT_ACCEPT);
    reqwest::Client::builder()
        .user_agent(user_agent)
        .default_headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::ACCEPT,
            accept,
        )]))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if redirect_refusal(attempt.previous(), attempt.url()).is_some() {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
}

#[cfg(test)]
mod tests {
    use super::{MAX_REDIRECTS, is_downgrade, redirect_refusal};
    use rstest::rstest;

    fn url(text: &str) -> url::Url {
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

    #[cfg(feature = "reqwest")]
    mod client {
        use super::super::{DEFAULT_ACCEPT, MAX_REDIRECTS, builder};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        /// A server answering every request with `response`; its base URL and
        /// the head of each request it had, lower-cased.
        async fn serve(response: &'static [u8]) -> (String, Arc<Mutex<Vec<String>>>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let base = format!("http://{}/", listener.local_addr().expect("addr"));
            let heads = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&heads);
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut buffer = [0_u8; 4096];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase();
                    seen.lock().expect("lock").push(head);
                    let _ = stream.write_all(response).await;
                }
            });
            (base, heads)
        }

        /// A request names the server and has the `Accept` list; a request's
        /// own `Accept` replaces the list.
        #[tokio::test]
        async fn a_request_has_the_default_client_headers() {
            let (base, heads) =
                serve(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            let client = builder("Ferrofin/test").build().expect("client");
            client.get(&base).send().await.expect("sent");
            client
                .get(&base)
                .header(reqwest::header::ACCEPT, "text/xml")
                .send()
                .await
                .expect("sent");

            let heads = heads.lock().expect("lock").clone();
            assert!(
                heads[0].contains("\r\nuser-agent: ferrofin/test\r\n"),
                "{}",
                heads[0]
            );
            assert!(
                heads[0].contains(&format!("\r\naccept: {DEFAULT_ACCEPT}\r\n")),
                "{}",
                heads[0]
            );
            assert!(
                heads[1].contains("\r\naccept: text/xml\r\n"),
                "{}",
                heads[1]
            );
            assert_eq!(heads[1].matches("\r\naccept:").count(), 1, "{}", heads[1]);
        }

        /// A redirect past the limit is not followed and not an error: its
        /// 3xx is the response, after the URL asked for and ten redirects.
        #[tokio::test]
        async fn a_redirect_past_the_limit_is_the_response() {
            let (base, heads) = serve(
                b"HTTP/1.1 302 Found\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
            let response = builder("Ferrofin/test")
                .build()
                .expect("client")
                .get(&base)
                .send()
                .await
                .expect("the 3xx is an answer");
            assert_eq!(response.status(), reqwest::StatusCode::FOUND);
            assert_eq!(heads.lock().expect("lock").len(), MAX_REDIRECTS + 1);
        }
    }
}
