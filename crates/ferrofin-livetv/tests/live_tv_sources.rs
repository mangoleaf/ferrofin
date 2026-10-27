//! How Live TV sources are opened: the remote-or-local rule, the M3U tuner's
//! save-time check and the HDHomeRun lineup address.
//!
//! Upstream decides "remote" with `StartsWith("http",
//! StringComparison.OrdinalIgnoreCase)` everywhere (v12.2 M3uParser.cs:52,
//! XmlTvListingsProvider.cs:76,343), validates an M3U tuner by opening its
//! playlist before saving it (`M3UTunerHost.Validate`, M3UTunerHost.cs:133-138,
//! run by `TunerHostManager.SaveTunerHost`), and fetches a HDHomeRun lineup
//! with `HttpClient`, which accepts only HTTP(S) URIs (HdHomerunHost.cs:77).

use std::io::{Read as _, Write as _};
use std::sync::Arc;

use ferrofin_livetv::fetch::{ReqwestFetcher, SourceFetcher};
use ferrofin_livetv::hdhomerun::HdHomerunHost;
use ferrofin_livetv::tuner_host::{M3uTunerHost, TunerHost};
use ferrofin_model::live_tv::TunerHostInfo;
use ferrofin_traits::error::ServiceError;
use rstest::rstest;

/// Serves one request with `status` and `body` on a fresh local port and
/// answers the `host:port` it listens on.
fn serve_once(status: &'static str, body: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    address
}

fn m3u_tuner(url: &str) -> TunerHostInfo {
    TunerHostInfo {
        type_: Some("m3u".to_owned()),
        url: Some(url.to_owned()),
        ..TunerHostInfo::default()
    }
}

/// A scheme in any case is a URL, not a file named `HTTP:…`.
#[rstest]
#[case("http")]
#[case("HTTP")]
#[case("Http")]
#[tokio::test]
async fn a_url_scheme_is_matched_ignoring_case(#[case] scheme: &str) {
    let address = serve_once("200 OK", "#EXTM3U\n");
    let body = ReqwestFetcher::new()
        .fetch(&format!("{scheme}://{address}/list.m3u"))
        .await
        .expect("fetched over HTTP");
    assert_eq!(body, "#EXTM3U\n");
}

/// `Validate`: a local playlist that does not exist is upstream's
/// `FileNotFoundException`, a `404`.
#[tokio::test]
async fn an_m3u_tuner_with_a_missing_playlist_file_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing.m3u");
    let host = M3uTunerHost::new(Arc::new(ReqwestFetcher::new()));
    let err = host
        .validate(&mut m3u_tuner(missing.to_str().expect("utf-8")))
        .await
        .expect_err("refused");
    assert!(matches!(err, ServiceError::NotFound(_)), "{err:?}");
}

/// A local source is opened exactly as given: `AsyncFile.OpenRead(info.Url)`
/// takes `file:///…` as a relative path, which does not exist, so the tuner
/// is refused with upstream's `FileNotFoundException`, a `404`, even though
/// the file the URL names is there.
#[tokio::test]
async fn an_m3u_tuner_given_as_a_file_url_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist = dir.path().join("list.m3u");
    std::fs::write(&playlist, "#EXTM3U\n").expect("write");
    let url = format!("file://{}", playlist.to_str().expect("utf-8"));
    let host = M3uTunerHost::new(Arc::new(ReqwestFetcher::new()));
    let err = host
        .validate(&mut m3u_tuner(&url))
        .await
        .expect_err("refused");
    assert!(matches!(err, ServiceError::NotFound(_)), "{err:?}");
}

/// The other readers take a local source as given too
/// (`XmlTvListingsProvider.GetXml`'s `AsyncFile.OpenRead(info.Path)`).
#[tokio::test]
async fn a_file_url_names_no_local_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let guide = dir.path().join("guide.xml");
    std::fs::write(&guide, "<tv/>").expect("write");
    let url = format!("file://{}", guide.to_str().expect("utf-8"));
    let fetcher = ReqwestFetcher::new();
    assert!(fetcher.fetch(&url).await.is_err(), "fetch");
    assert!(fetcher.fetch_bytes(&url).await.is_err(), "fetch_bytes");
    assert!(fetcher.open(&url, None).await.is_err(), "open");
    assert!(
        fetcher.fetch_listings(&url).await.is_err(),
        "fetch_listings"
    );
}

/// `Validate`: a directory is no playlist; `AsyncFile.OpenRead` refuses it
/// (`UnauthorizedAccessException`, a `500`).
#[tokio::test]
async fn an_m3u_tuner_whose_playlist_path_is_a_directory_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = M3uTunerHost::new(Arc::new(ReqwestFetcher::new()));
    let err = host
        .validate(&mut m3u_tuner(dir.path().to_str().expect("utf-8")))
        .await
        .expect_err("refused");
    assert!(matches!(err, ServiceError::BackendSource(_)), "{err:?}");
}

/// `Validate`: a playlist URL answering a failure status is
/// `EnsureSuccessStatusCode`'s `HttpRequestException`, a `500`.
#[tokio::test]
async fn an_m3u_tuner_whose_playlist_url_fails_is_refused() {
    let address = serve_once("503 Service Unavailable", "");
    let host = M3uTunerHost::new(Arc::new(ReqwestFetcher::new()));
    let err = host
        .validate(&mut m3u_tuner(&format!("HTTP://{address}/list.m3u")))
        .await
        .expect_err("refused");
    assert!(
        matches!(&err, ServiceError::Backend(message) if message.contains("503")),
        "{err:?}"
    );
}

/// `Validate` accepts a playlist that opens, a file or a URL in any case.
#[tokio::test]
async fn an_m3u_tuner_whose_playlist_opens_is_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist = dir.path().join("list.m3u");
    std::fs::write(&playlist, "#EXTM3U\n").expect("write");
    let host = M3uTunerHost::new(Arc::new(ReqwestFetcher::new()));
    host.validate(&mut m3u_tuner(playlist.to_str().expect("utf-8")))
        .await
        .expect("a file");

    let address = serve_once("200 OK", "#EXTM3U\n");
    host.validate(&mut m3u_tuner(&format!("Http://{address}/list.m3u")))
        .await
        .expect("a URL");
}

/// A device whose `discover.json` names `lineup` as its `LineupURL`, and
/// which would serve that path as a file if asked.
struct LineupAt(String);

#[async_trait::async_trait]
impl SourceFetcher for LineupAt {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        if url.ends_with("/discover.json") {
            return Ok(serde_json::json!({
                "ModelNumber": "HDHR5-4K",
                "DeviceID": "12345678",
                "LineupURL": self.0,
            })
            .to_string());
        }
        std::fs::read_to_string(url).map_err(|e| ServiceError::backend(e.to_string()))
    }
}

/// `GetLineup` requests the lineup over HTTP only: a device naming a local
/// file never has it read as the lineup.
#[tokio::test]
async fn a_hdhomerun_lineup_address_that_names_a_local_file_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lineup = dir.path().join("lineup.json");
    std::fs::write(
        &lineup,
        r#"[{"GuideNumber":"1","GuideName":"Local","URL":"http://x/1"}]"#,
    )
    .expect("write");
    let host = HdHomerunHost::new(Arc::new(LineupAt(
        lineup.to_str().expect("utf-8").to_owned(),
    )));
    let tuner = TunerHostInfo {
        url: Some("192.168.1.182".to_owned()),
        ..TunerHostInfo::default()
    };
    let err = host.get_lineup(&tuner).await.expect_err("refused");
    assert!(matches!(err, ServiceError::Backend(_)), "{err:?}");
}
