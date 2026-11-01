//! A tuner or guide source's URL can carry the account: in its userinfo, in
//! its query (Xtream's `get.php?username=…&password=…`) and in its path
//! (`/live/<user>/<pass>/1.ts`). A failed source shows none of that in its
//! error, whose text reaches the logs and, for a `4xx`, the response body. The
//! error names the source by its scheme and host instead.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_livetv::fetch::{
    RequestMethod, ReqwestFetcher, SourceBody, SourceFetcher, SourceRequest,
};
use ferrofin_livetv::hdhomerun::HdHomerunHost;
use ferrofin_livetv::stream::{ReqwestTunerSource, TunerStreamSource, open_shared_http_stream};
use ferrofin_livetv::tuner_host::{M3uTunerHost, TunerHost};
use ferrofin_livetv::xmltv_cache::XmlTvCache;
use ferrofin_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
use ferrofin_traits::error::ServiceError;
use rstest::rstest;

/// What [`source`] puts in the URL. None of it may be shown.
const ACCOUNT: [&str; 4] = ["acct-user", "info-secret", "path-secret", "query-secret"];

/// A source at `address` with the account in its userinfo, path and query.
fn source(address: &str, file: &str) -> String {
    format!(
        "http://acct-user:info-secret@{address}/live/acct-user/path-secret/{file}\
         ?username=acct-user&password=query-secret"
    )
}

/// An address that refuses every connection.
const REFUSED: &str = "127.0.0.1:0";

/// Serves `response`, written as it is, to every request on a fresh local port
/// and answers the address.
fn serve(response: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    address
}

/// `error`'s text, what `Debug` prints and its causes show none of the account.
#[track_caller]
fn assert_no_account(error: &(dyn std::error::Error + 'static), debug: &str) {
    let mut shown = vec![debug.to_owned()];
    let mut cause = Some(error);
    while let Some(current) = cause {
        shown.push(current.to_string());
        cause = current.source();
    }
    let shown = shown.join(" | ");
    for secret in ACCOUNT {
        assert!(!shown.contains(secret), "{secret} shown in: {shown}");
    }
}

/// `error` names the source at `address` by its scheme and host, and nothing more.
#[track_caller]
fn assert_names_host(error: &ServiceError, address: &str) {
    assert_no_account(error, &format!("{error:?}"));
    let text = error.to_string();
    assert!(text.contains(&format!("http://{address}")), "{text}");
}

fn m3u_tuner(url: &str) -> TunerHostInfo {
    TunerHostInfo {
        type_: Some("m3u".to_owned()),
        url: Some(url.to_owned()),
        ..TunerHostInfo::default()
    }
}

fn hdhomerun_tuner(url: &str) -> TunerHostInfo {
    TunerHostInfo {
        type_: Some("hdhomerun".to_owned()),
        url: Some(url.to_owned()),
        ..TunerHostInfo::default()
    }
}

fn xmltv_provider(url: &str) -> ListingsProviderInfo {
    ListingsProviderInfo {
        id: Some(uuid::Uuid::new_v4().simple().to_string()),
        type_: Some("xmltv".to_owned()),
        path: Some(url.to_owned()),
        ..ListingsProviderInfo::default()
    }
}

#[tokio::test]
async fn a_refused_source_is_named_by_its_host() {
    let url = source(REFUSED, "list.m3u");
    let fetcher = ReqwestFetcher::new();

    let error = fetcher.fetch(&url).await.expect_err("fetch");
    assert_names_host(&error, REFUSED);
    let error = fetcher.fetch_bytes(&url).await.expect_err("fetch_bytes");
    assert_names_host(&error, REFUSED);
    let error = fetcher.open(&url, None).await.expect_err("open");
    assert_names_host(&error, REFUSED);
    let error = fetcher.fetch_listings(&url).await.err().expect("listings");
    assert_names_host(&error, REFUSED);
    let request = SourceRequest::new(RequestMethod::Get, url.clone());
    let error = fetcher.send(request).await.expect_err("send");
    assert_names_host(&error, REFUSED);
}

#[tokio::test]
async fn a_failure_status_is_named_by_its_host() {
    let address = serve("HTTP/1.1 500 Oops\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let url = source(&address, "list.m3u");
    let fetcher = ReqwestFetcher::new();

    let error = fetcher.fetch(&url).await.expect_err("fetch");
    assert_names_host(&error, &address);
    let error = fetcher.fetch_bytes(&url).await.expect_err("fetch_bytes");
    assert_names_host(&error, &address);
    let error = fetcher.fetch_listings(&url).await.err().expect("listings");
    assert_names_host(&error, &address);
}

#[tokio::test]
async fn a_body_cut_short_is_named_by_its_host() {
    let address = serve("HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\nshort");
    let url = source(&address, "list.m3u");
    let fetcher = ReqwestFetcher::new();

    let error = fetcher.fetch(&url).await.expect_err("fetch");
    assert_names_host(&error, &address);
    let error = fetcher.fetch_bytes(&url).await.expect_err("fetch_bytes");
    assert_names_host(&error, &address);
    let opened = fetcher.open(&url, None).await.expect("open");
    let error = opened.into_bytes().await.expect_err("read");
    assert_names_host(&error, &address);
}

/// A gzip failure names the URL after redirects, which is not the configured
/// one and can carry an account of its own.
#[tokio::test]
async fn a_gzipped_guide_that_does_not_extract_is_named_by_its_host() {
    let address = serve("HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\nnot gz");
    let url = source(&address, "guide.xml.gz");

    let mut body = ReqwestFetcher::new()
        .fetch_listings(&url)
        .await
        .expect("the response opened");
    let error = loop {
        match body.next_chunk().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("not gzip, but it extracted"),
            Err(error) => break error,
        }
    };
    assert_names_host(&error, &address);
}

/// What the trait's defaults answer for a text-only fetcher, which every test
/// fake and a future source build on.
struct AnswersFailure;

#[async_trait]
impl SourceFetcher for AnswersFailure {
    async fn fetch(&self, _url: &str) -> Result<String, ServiceError> {
        Ok(String::new())
    }

    async fn open(&self, _url: &str, _agent: Option<&str>) -> Result<SourceBody, ServiceError> {
        Ok(SourceBody::from_bytes(503, Vec::new()))
    }
}

#[tokio::test]
async fn the_fetcher_defaults_name_a_source_by_its_host() {
    let url = source("guide.example", "guide.xml");

    let error = AnswersFailure
        .fetch_listings(&url)
        .await
        .err()
        .expect("a failure status");
    assert_names_host(&error, "guide.example");
    let request = SourceRequest::new(RequestMethod::Post, url.clone());
    let error = AnswersFailure.send(request).await.expect_err("refused");
    assert_names_host(&error, "guide.example");
}

#[tokio::test]
async fn an_m3u_tuner_answering_a_failure_is_named_by_its_host() {
    let address = serve("HTTP/1.1 502 Oops\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let tuner = m3u_tuner(&source(&address, "playlist.m3u"));

    let error = M3uTunerHost::new(Arc::new(ReqwestFetcher::new()))
        .get_channels(&tuner)
        .await
        .expect_err("a failure status");
    assert_names_host(&error, &address);
}

/// A scheme `is_http` does not know is read as a file, and the missing file is
/// a `404`: the one failure whose text reaches the response body.
#[rstest]
#[case("rtsp")]
#[case("ftp")]
#[tokio::test]
async fn a_source_read_as_a_file_is_a_404_showing_no_account(#[case] scheme: &str) {
    let url = format!("{scheme}://acct-user:info-secret@tuner.example:554/path-secret/list.m3u");

    let error = M3uTunerHost::new(Arc::new(ReqwestFetcher::new()))
        .get_channels(&m3u_tuner(&url))
        .await
        .expect_err("no such file");

    assert!(matches!(error, ServiceError::NotFound(_)), "{error:?}");
    assert_no_account(&error, &format!("{error:?}"));
    let text = error.to_string();
    assert!(
        text.contains(&format!("{scheme}://tuner.example:554")),
        "{text}"
    );
}

/// The file `read` failure itself keeps a local path: it is the diagnostic, and
/// a filesystem path is fine in a log (`LOGGING.md`, rule 5).
#[tokio::test]
async fn a_missing_local_source_keeps_its_path() {
    let error = ReqwestFetcher::new()
        .fetch("/nonexistent/ferrofin/livetv/guide.xml")
        .await
        .expect_err("no such file");

    assert!(
        error
            .to_string()
            .contains("/nonexistent/ferrofin/livetv/guide.xml"),
        "{error}"
    );
}

#[tokio::test]
async fn a_hdhomerun_device_answering_a_failure_is_named_by_its_host() {
    let address = serve("HTTP/1.1 500 Oops\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let tuner = hdhomerun_tuner(&format!("http://acct-user:info-secret@{address}"));

    let error = HdHomerunHost::new(Arc::new(ReqwestFetcher::new()))
        .get_model_info(&tuner, true)
        .await
        .expect_err("a failure status");
    assert_names_host(&error, &address);
}

#[tokio::test]
async fn a_hdhomerun_device_answering_no_json_is_named_by_its_host() {
    let address = serve("HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\nnope");
    let tuner = hdhomerun_tuner(&format!("http://acct-user:info-secret@{address}"));

    let error = HdHomerunHost::new(Arc::new(ReqwestFetcher::new()))
        .get_model_info(&tuner, true)
        .await
        .expect_err("not JSON");
    assert_names_host(&error, &address);
}

/// `LineupURL` comes from the device. An address that is not HTTP is refused
/// and the refusal shows no account; one that answers no JSON fails the parse.
#[rstest]
#[case::not_http(
    r#"{"LineupURL":"ftp://acct-user:info-secret@device.example/path-secret/lineup"}"#,
    "ftp://device.example"
)]
#[case::not_json(
    r#"{"LineupURL":"http://acct-user:info-secret@device.example:9/path-secret?x=query-secret"}"#,
    "http://device.example:9"
)]
#[tokio::test]
async fn a_hdhomerun_lineup_address_is_named_by_its_host(
    #[case] discover: &'static str,
    #[case] shown: &str,
) {
    struct Device(&'static str);

    #[async_trait]
    impl SourceFetcher for Device {
        async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
            Ok(if url.ends_with("/discover.json") {
                self.0.to_owned()
            } else {
                "not json".to_owned()
            })
        }
    }

    let error = HdHomerunHost::new(Arc::new(Device(discover)))
        .get_lineup(&hdhomerun_tuner("http://device.example"))
        .await
        .expect_err("no lineup");

    assert_no_account(&error, &format!("{error:?}"));
    assert!(error.to_string().contains(shown), "{error}");
}

#[tokio::test]
async fn a_refused_tuner_stream_is_named_by_its_host() {
    let url = source(REFUSED, "1.ts");

    let error = ReqwestTunerSource::new()
        .open(&url, &HashMap::new())
        .await
        .err()
        .expect("refused");
    assert_names_host(&error, REFUSED);
}

#[tokio::test]
async fn a_tuner_stream_answering_a_failure_is_named_by_its_host() {
    let address = serve("HTTP/1.1 500 Oops\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let url = source(&address, "1.ts");

    let error = ReqwestTunerSource::new()
        .open(&url, &HashMap::new())
        .await
        .err()
        .expect("a failure status");
    assert_names_host(&error, &address);
}

/// `copy_to_temp_file` and `record_direct` log the body's error as it is.
#[tokio::test]
async fn a_tuner_stream_cut_short_shows_no_account() {
    let address = serve("HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\nshort");
    let url = source(&address, "1.ts");

    let mut body = ReqwestTunerSource::new()
        .open(&url, &HashMap::new())
        .await
        .expect("open");
    let error = loop {
        match body.next_chunk().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream was cut short"),
            Err(error) => break error,
        }
    };
    assert_no_account(&error, &format!("{error:?}"));
}

#[tokio::test]
async fn a_tuner_stream_that_sends_nothing_is_named_by_its_host() {
    let address = serve("HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let url = source(&address, "1.ts");
    let dir = tempfile::tempdir().expect("tempdir");

    let error = open_shared_http_stream(
        &ReqwestTunerSource::new(),
        &url,
        &HashMap::new(),
        dir.path(),
    )
    .await
    .expect_err("zero bytes");
    assert_names_host(&error, &address);
}

#[tokio::test]
async fn a_guide_that_failed_recently_is_named_by_its_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = XmlTvCache::new(Arc::new(ReqwestFetcher::new()), dir.path());
    let provider = xmltv_provider(&source(REFUSED, "xmltv.php"));

    let error = cache.get_xml(&provider).await.expect_err("refused");
    assert_names_host(&error, REFUSED);
    let error = cache.get_xml(&provider).await.expect_err("backed off");
    assert!(error.to_string().contains("Skipping"), "{error}");
    assert_names_host(&error, REFUSED);
}

#[tokio::test]
async fn an_empty_guide_is_named_by_its_host() {
    let address = serve("HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = XmlTvCache::new(Arc::new(ReqwestFetcher::new()), dir.path());

    let error = cache
        .get_xml(&xmltv_provider(&source(&address, "xmltv.php")))
        .await
        .expect_err("empty");
    assert!(error.to_string().contains("empty"), "{error}");
    assert_names_host(&error, &address);
}
