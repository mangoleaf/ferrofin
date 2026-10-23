//! The source-fetch seam.
//!
//! `refresh_guide` needs to read the configured M3U and XMLTV URLs. That network
//! I/O sits behind [`SourceFetcher`] so the manager's channel/guide-population
//! logic can be unit-tested with an in-memory fake instead of a live server.

use async_trait::async_trait;

use ferrofin_traits::error::ServiceError;

use crate::error::LiveTvError;

/// Fetches the body of a tuner/guide/listings source URL (or local file path).
#[async_trait]
pub trait SourceFetcher: Send + Sync {
    /// Fetches `url` and returns its body as text.
    async fn fetch(&self, url: &str) -> Result<String, ServiceError>;

    /// Fetches `url` presenting `user_agent` when one is given
    /// (`M3uParser.GetListingsStream`: `requestMessage.Headers.UserAgent.
    /// TryParseAdd(info.UserAgent)` for a non-empty configured value, so a
    /// playlist host that only serves a particular client still answers).
    ///
    /// The default ignores the agent and delegates to [`fetch`](Self::fetch),
    /// so a text-only fake keeps working; the real fetcher sets the header.
    async fn fetch_as(&self, url: &str, user_agent: Option<&str>) -> Result<String, ServiceError> {
        let _ = user_agent;
        self.fetch(url).await
    }

    /// Fetches `url` and returns its body as raw bytes — for documents the
    /// server passes through untouched (the Schedules Direct country list).
    ///
    /// The default goes through [`fetch`](Self::fetch), so a text-only fake
    /// serves both; the real fetcher reads the response bytes directly.
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ServiceError> {
        self.fetch(url).await.map(String::into_bytes)
    }

    /// Fetches `url` and returns `(status, body)` **without** failing on a
    /// non-success status.
    ///
    /// The HDHomeRun host needs the code itself, not just success-or-not:
    /// `HdHomerunHost.GetModelInfo` treats a **404** as "this is an HDHR4,
    /// which has no `discover.json`" and answers with a synthetic model rather
    /// than an error (v10.11.8 HdHomerunHost.cs:135-157), while every other
    /// failure propagates. Flattening that to one error variant would turn a
    /// perfectly good tuner into a broken one.
    ///
    /// The default reports `200` for whatever [`fetch`](Self::fetch) returns,
    /// so a text-only fake keeps working; a fake that wants to exercise the
    /// 404 branch overrides this.
    async fn fetch_with_status(&self, url: &str) -> Result<(u16, String), ServiceError> {
        self.fetch(url).await.map(|body| (200, body))
    }

    /// Fetches an XMLTV listings document: [`fetch`](Self::fetch), except that
    /// a gzipped source is decompressed. Port of
    /// `XmlTvListingsProvider.UnzipIfNeededAndCopy`, which decides on the URL
    /// after redirects, or the path, with any query stripped: `.gz` or
    /// `.gzip`.
    ///
    /// The default is plain [`fetch`](Self::fetch), so a text-only fake keeps
    /// working; the real fetcher decompresses.
    async fn fetch_listings(&self, url: &str) -> Result<String, ServiceError> {
        self.fetch(url).await
    }
}

/// The `User-Agent` the real fetcher presents, mirroring the product header
/// Jellyfin's default `HttpClient` sends (`Jellyfin-Server/<version>`,
/// `Jellyfin.Server/Startup.cs`). A generic product token only — never
/// anything identifying the user or the install.
const USER_AGENT: &str = concat!("Ferrofin/", env!("CARGO_PKG_VERSION"));

/// The real fetcher: `reqwest` for `http(s)://`, a filesystem read otherwise.
#[derive(Debug, Clone)]
pub struct ReqwestFetcher {
    client: reqwest::Client,
}

impl Default for ReqwestFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestFetcher {
    /// Creates a fetcher over a fresh `reqwest` client presenting the
    /// [`USER_AGENT`] product header.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            // A builder failure here means the TLS backend could not
            // initialise; the plain client surfaces the same condition on
            // first use, where it is reported per fetch rather than at wiring.
            .unwrap_or_default();
        Self { client }
    }

    /// Performs the `GET`, failing on transport errors and non-success statuses.
    async fn get(&self, url: &str) -> Result<reqwest::Response, ServiceError> {
        self.get_as(url, None).await
    }

    /// [`get`](Self::get) with a per-request `User-Agent`, when it is a valid
    /// header value; an invalid one is dropped, as `TryParseAdd` drops it.
    async fn get_as(
        &self,
        url: &str,
        user_agent: Option<&str>,
    ) -> Result<reqwest::Response, ServiceError> {
        let mut request = self.client.get(url);
        if let Some(agent) = user_agent.map(str::trim).filter(|agent| !agent.is_empty())
            && let Ok(value) = reqwest::header::HeaderValue::from_str(agent)
        {
            request = request.header(reqwest::header::USER_AGENT, value);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| LiveTvError::http(format!("fetch {url}"), e))?;
        Ok(resp
            .error_for_status()
            .map_err(|e| LiveTvError::http(format!("fetch {url}"), e))?)
    }
}

/// Whether `url` names a remote `http(s)://` source (vs. a local path).
fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Whether a listings source is gzipped, as `UnzipIfNeededAndCopy` decides:
/// the extension of `name` (a URL after redirects, or a path) with any query
/// stripped is `.gz` or `.gzip`, ignoring case.
fn is_gzip_name(name: &str) -> bool {
    let name = name.split('?').next().unwrap_or(name);
    std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("gzip")
        })
}

/// Decompresses a gzipped listings document and decodes it as UTF-8 (lossily,
/// like `.text()` on a response that names no charset). Every gzip member is
/// read, as .NET's `GZipStream` reads concatenated members. It runs on the
/// blocking pool, because an EPG can run to tens of megabytes.
async fn gunzip<B>(name: &str, bytes: B) -> Result<String, ServiceError>
where
    B: AsRef<[u8]> + Send + 'static,
{
    let context = format!("extract gz {name}");
    let text = tokio::task::spawn_blocking(move || {
        let mut text = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::MultiGzDecoder::new(bytes.as_ref()),
            &mut text,
        )
        .map(|_| text)
    })
    .await
    .map_err(|e| LiveTvError::io(context.clone(), std::io::Error::other(e)))?
    .map_err(|e| LiveTvError::io(context, e))?;
    Ok(String::from_utf8(text)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

#[async_trait]
impl SourceFetcher for ReqwestFetcher {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        if is_http(url) {
            self.get(url)
                .await?
                .text()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e).into())
        } else {
            let path = url.strip_prefix("file://").unwrap_or(url);
            tokio::fs::read_to_string(path)
                .await
                .map_err(|e| LiveTvError::io(format!("read {path}"), e).into())
        }
    }

    async fn fetch_as(&self, url: &str, user_agent: Option<&str>) -> Result<String, ServiceError> {
        if is_http(url) {
            self.get_as(url, user_agent)
                .await?
                .text()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e).into())
        } else {
            self.fetch(url).await
        }
    }

    async fn fetch_with_status(&self, url: &str) -> Result<(u16, String), ServiceError> {
        if is_http(url) {
            let resp = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| LiveTvError::http(format!("fetch {url}"), e))?;
            let status = resp.status().as_u16();
            let body = resp
                .text()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e))?;
            Ok((status, body))
        } else {
            // A local path has no status; a missing file is the 404 analogue,
            // which is exactly how the HDHR4 branch reads it.
            match self.fetch(url).await {
                Ok(body) => Ok((200, body)),
                Err(e) => {
                    let path = url.strip_prefix("file://").unwrap_or(url);
                    if tokio::fs::try_exists(path).await.unwrap_or(false) {
                        Err(e)
                    } else {
                        Ok((404, String::new()))
                    }
                }
            }
        }
    }

    async fn fetch_listings(&self, url: &str) -> Result<String, ServiceError> {
        if is_http(url) {
            let resp = self.get(url).await?;
            let final_url = resp.url().to_string();
            if !is_gzip_name(&final_url) {
                return resp
                    .text()
                    .await
                    .map_err(|e| LiveTvError::http(format!("read {url}"), e).into());
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e))?;
            gunzip(&final_url, bytes).await
        } else {
            let path = url.strip_prefix("file://").unwrap_or(url);
            if !is_gzip_name(path) {
                return self.fetch(url).await;
            }
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|e| LiveTvError::io(format!("read {path}"), e))?;
            gunzip(path, bytes).await
        }
    }

    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ServiceError> {
        if is_http(url) {
            self.get(url)
                .await?
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| LiveTvError::http(format!("read {url}"), e).into())
        } else {
            let path = url.strip_prefix("file://").unwrap_or(url);
            tokio::fs::read(path)
                .await
                .map_err(|e| LiveTvError::io(format!("read {path}"), e).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReqwestFetcher, SourceFetcher};

    #[tokio::test]
    async fn reads_a_local_file_path() {
        let dir = std::env::temp_dir();
        let path = dir.join("ferrofin_livetv_fetch_test.m3u");
        tokio::fs::write(&path, "#EXTM3U\n").await.expect("write");
        let body = ReqwestFetcher::new()
            .fetch(path.to_str().unwrap())
            .await
            .expect("fetch");
        assert_eq!(body, "#EXTM3U\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let err = ReqwestFetcher::new()
            .fetch("/nonexistent/ferrofin/livetv/nope.xml")
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn reads_bytes_from_a_local_file_path() {
        let dir = std::env::temp_dir();
        let path = dir.join("ferrofin_livetv_fetch_bytes_test.json");
        tokio::fs::write(&path, b"[{\"shortName\":\"USA\"}]")
            .await
            .expect("write");
        let body = ReqwestFetcher::new()
            .fetch_bytes(path.to_str().unwrap())
            .await
            .expect("fetch_bytes");
        assert_eq!(body, b"[{\"shortName\":\"USA\"}]");
        let _ = tokio::fs::remove_file(&path).await;

        let err = ReqwestFetcher::new()
            .fetch_bytes("/nonexistent/ferrofin/livetv/nope.json")
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn default_fetch_bytes_delegates_to_fetch() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl SourceFetcher for TextOnly {
            async fn fetch(
                &self,
                url: &str,
            ) -> Result<String, ferrofin_traits::error::ServiceError> {
                Ok(format!("body of {url}"))
            }
        }
        let bytes = TextOnly.fetch_bytes("x").await.expect("fetch_bytes");
        assert_eq!(bytes, b"body of x");
    }

    /// One-shot HTTP server that answers with `200 #EXTM3U` and reports the
    /// `User-Agent` header it received.
    fn serve_once() -> (String, std::sync::mpsc::Receiver<Option<String>>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/list.m3u", listener.local_addr().expect("addr"));
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0_u8; 4096];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let agent = request.lines().find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
                        .map(|(_, value)| value.trim().to_owned())
                });
                let _ = sender.send(agent);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\n#EXTM3U",
                );
            }
        });
        (url, receiver)
    }

    /// `M3uParser.GetListingsStream` sends the tuner's configured user agent;
    /// without one (or with a blank one) the client's own product agent goes.
    #[tokio::test]
    async fn a_configured_user_agent_is_sent_with_the_playlist_request() {
        for (configured, expected) in [
            (Some(" VLC/3.0 "), Some("VLC/3.0".to_owned())),
            (None, Some(super::USER_AGENT.to_owned())),
            (Some("   "), Some(super::USER_AGENT.to_owned())),
            // Not a valid header value: dropped, like `TryParseAdd`.
            (Some("bad\nagent"), Some(super::USER_AGENT.to_owned())),
        ] {
            let (url, received) = serve_once();
            let body = ReqwestFetcher::new()
                .fetch_as(&url, configured)
                .await
                .expect("fetch");
            assert_eq!(body, "#EXTM3U");
            assert_eq!(received.recv().expect("agent"), expected, "{configured:?}");
        }
    }

    #[tokio::test]
    async fn the_default_fetch_as_ignores_the_agent_and_a_file_needs_none() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl SourceFetcher for TextOnly {
            async fn fetch(
                &self,
                url: &str,
            ) -> Result<String, ferrofin_traits::error::ServiceError> {
                Ok(format!("body of {url}"))
            }
        }
        assert_eq!(
            TextOnly.fetch_as("x", Some("agent")).await.expect("fetch"),
            "body of x"
        );
        let path = std::env::temp_dir().join("ferrofin_livetv_fetch_as_test.m3u");
        tokio::fs::write(&path, "#EXTM3U\n").await.expect("write");
        let body = ReqwestFetcher::new()
            .fetch_as(path.to_str().unwrap(), Some("agent"))
            .await
            .expect("fetch");
        assert_eq!(body, "#EXTM3U\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    /// `UnzipIfNeededAndCopy`'s test: the extension, query stripped, is `.gz`
    /// or `.gzip` in any case.
    #[rstest::rstest]
    #[case("/epg/guide.xml.gz", true)]
    #[case("GUIDE.XML.GZIP", true)]
    #[case("http://host/epg.gz?token=1", true)]
    #[case("http://host/guide.xml", false)]
    #[case("http://host/guide.xml?name=a.gz", false)]
    #[case("http://host/epg.tgz", false)]
    fn gzip_is_decided_by_the_extension(#[case] name: &str, #[case] gzipped: bool) {
        assert_eq!(super::is_gzip_name(name), gzipped);
    }

    fn gzipped(text: &str) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(text.as_bytes()).expect("compress");
        encoder.finish().expect("finish")
    }

    /// A guide gzipped as several concatenated members (as some EPG builders
    /// write them) is read in full, as `GZipStream` reads it.
    #[tokio::test]
    async fn every_gzip_member_is_read() {
        let dir = tempfile::tempdir().expect("tmp");
        let gz = dir.path().join("guide.xml.gz");
        let mut members = gzipped("<tv>");
        members.extend(gzipped("</tv>"));
        tokio::fs::write(&gz, members).await.expect("write");
        assert_eq!(
            ReqwestFetcher::new()
                .fetch_listings(gz.to_str().unwrap())
                .await
                .expect("gz"),
            "<tv></tv>"
        );
    }

    /// A local `.xml.gz` guide is read decompressed; a plain one as it is.
    #[tokio::test]
    async fn a_local_gzipped_guide_is_decompressed() {
        let dir = tempfile::tempdir().expect("tmp");
        let gz = dir.path().join("guide.xml.gz");
        tokio::fs::write(&gz, gzipped("<tv></tv>"))
            .await
            .expect("write");
        let fetcher = ReqwestFetcher::new();
        assert_eq!(
            fetcher
                .fetch_listings(gz.to_str().unwrap())
                .await
                .expect("gz"),
            "<tv></tv>"
        );
        let plain = dir.path().join("guide.xml");
        tokio::fs::write(&plain, "<tv/>").await.expect("write");
        assert_eq!(
            fetcher
                .fetch_listings(plain.to_str().unwrap())
                .await
                .expect("plain"),
            "<tv/>"
        );
        // Not gzip after all: an extraction error, not a garbled guide.
        let fake = dir.path().join("fake.xml.gz");
        tokio::fs::write(&fake, "<tv/>").await.expect("write");
        assert!(
            fetcher
                .fetch_listings(fake.to_str().unwrap())
                .await
                .is_err()
        );
    }

    /// Answers each connection in turn with the next canned response.
    fn serve_sequence(responses: Vec<Vec<u8>>) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for response in responses {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer);
                    let _ = stream.write_all(&response);
                }
            }
        });
        base
    }

    /// `UnzipIfNeededAndCopy` decides on the URL after redirects: a feed URL
    /// without an extension that redirects to `guide.xml.gz` is decompressed.
    #[tokio::test]
    async fn a_redirect_to_a_gzipped_guide_is_decompressed() {
        let body = gzipped("<tv><channel id=\"a\"/></tv>");
        let mut ok = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        ok.extend_from_slice(&body);
        let base = serve_sequence(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /guide.xml.gz\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ok,
        ]);
        let text = ReqwestFetcher::new()
            .fetch_listings(&format!("{base}/epg?id=1"))
            .await
            .expect("fetch");
        assert_eq!(text, "<tv><channel id=\"a\"/></tv>");
    }

    #[tokio::test]
    async fn the_default_fetch_listings_delegates_to_fetch() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl SourceFetcher for TextOnly {
            async fn fetch(
                &self,
                url: &str,
            ) -> Result<String, ferrofin_traits::error::ServiceError> {
                Ok(format!("body of {url}"))
            }
        }
        assert_eq!(
            TextOnly.fetch_listings("x.gz").await.expect("fetch"),
            "body of x.gz"
        );
    }

    #[tokio::test]
    async fn http_fetch_bytes_fails_on_an_unreachable_host() {
        let err = ReqwestFetcher::new()
            .fetch_bytes("http://127.0.0.1:0/countries")
            .await
            .expect_err("connect must fail");
        assert!(
            err.to_string()
                .contains("fetch http://127.0.0.1:0/countries")
        );
    }
}
