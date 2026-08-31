//! `HdHomerunHostTests` (v10.11.8 `tests/Jellyfin.LiveTv.Tests/HdHomerunHostTests.cs`),
//! transliterated.
//!
//! Upstream's fixture mocks `HttpMessageHandler` so that a request is answered
//! from `Test Data/LiveTv/{RequestUri.Host}/{last segment}`. Ferrofin's
//! equivalent seam is [`SourceFetcher`], so [`FixtureDevice`] does the same
//! lookup over the same JSON files, vendored byte-for-byte into
//! `tests/data/hdhomerun/`. The C# expected values are the oracle: every
//! assertion below is one of upstream's seven `[Fact]`s.
//!
//! The one thing a fetcher-level fake cannot exercise is the HTTP status the
//! HDHR4 fallback keys on, so `hdhr4_*` drives the real [`ReqwestFetcher`]
//! against a local `TcpListener` instead.

use std::sync::Arc;

use ferrofin_livetv::fetch::{ReqwestFetcher, SourceFetcher};
use ferrofin_livetv::hdhomerun::HdHomerunHost;
use ferrofin_livetv::tuner_host::TunerHost;
use ferrofin_model::live_tv::TunerHostInfo;
use ferrofin_traits::error::ServiceError;

/// Serves the vendored fixtures the way upstream's mocked handler does:
/// `{host}/{last path segment}`.
struct FixtureDevice;

#[async_trait::async_trait]
impl SourceFetcher for FixtureDevice {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let (host, segment) = rest
            .split_once('/')
            .ok_or_else(|| ServiceError::not_found(url.to_owned()))?;
        let host = host.split(':').next().unwrap_or(host);
        let segment = segment.rsplit('/').next().unwrap_or(segment);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/hdhomerun")
            .join(host)
            .join(segment);
        std::fs::read_to_string(&path)
            .map_err(|e| ServiceError::backend(format!("{}: {e}", path.display())))
    }
}

fn host() -> HdHomerunHost {
    HdHomerunHost::new(Arc::new(FixtureDevice))
}

fn tuner(url: &str) -> TunerHostInfo {
    TunerHostInfo {
        url: Some(url.to_owned()),
        ..TunerHostInfo::default()
    }
}

/// `GetModelInfo_Valid_Success`.
#[tokio::test]
async fn get_model_info_valid_success() {
    let model = host()
        .get_model_info(&tuner("192.168.1.182"), true)
        .await
        .expect("model info");
    assert_eq!(model.friendly_name.as_deref(), Some("HDHomeRun PRIME"));
    assert_eq!(model.model_number.as_deref(), Some("HDHR3-CC"));
    assert_eq!(model.firmware_name.as_deref(), Some("hdhomerun3_cablecard"));
    assert_eq!(model.firmware_version.as_deref(), Some("20160630atest2"));
    assert_eq!(model.device_id.as_deref(), Some("FFFFFFFF"));
    assert_eq!(model.device_auth.as_deref(), Some("FFFFFFFF"));
    assert_eq!(model.tuner_count, 3);
    assert_eq!(model.base_url.as_deref(), Some("http://192.168.1.182:80"));
    assert_eq!(
        model.lineup_url.as_deref(),
        Some("http://192.168.1.182:80/lineup.json")
    );
    // Not an upstream assertion, but the consequence the whole streaming path
    // turns on: an HDHR3 is not an EXTEND, so only the native profile exists.
    assert!(!model.supports_transcoding());
}

/// `GetModelInfo_Legacy_Success`.
#[tokio::test]
async fn get_model_info_legacy_success() {
    let model = host()
        .get_model_info(&tuner("10.10.10.100"), true)
        .await
        .expect("model info");
    assert_eq!(model.friendly_name.as_deref(), Some("HDHomeRun DUAL"));
    assert_eq!(model.model_number.as_deref(), Some("HDHR3-US"));
    assert_eq!(model.firmware_name.as_deref(), Some("hdhomerun3_atsc"));
    assert_eq!(model.firmware_version.as_deref(), Some("20200225"));
    assert_eq!(model.device_id.as_deref(), Some("10xxxxx5"));
    assert_eq!(model.device_auth, None);
    assert_eq!(model.tuner_count, 2);
    assert_eq!(model.base_url.as_deref(), Some("http://10.10.10.100:80"));
    assert_eq!(model.lineup_url, None);
}

/// `GetModelInfo_EmptyUrl_ArgumentException`.
#[tokio::test]
async fn get_model_info_empty_url_argument_exception() {
    let err = host()
        .get_model_info(&tuner(""), true)
        .await
        .expect_err("empty url must be rejected");
    assert!(
        matches!(err, ServiceError::InvalidInput(ref m) if m == "Invalid tuner info"),
        "{err}"
    );
    // And it is rejected BEFORE the request, so `throwAllExceptions: false`
    // does not turn it into the HDHR4 fallback.
    assert!(host().get_model_info(&tuner(""), false).await.is_err());
}

/// `GetLineup_Valid_Success`.
#[tokio::test]
async fn get_lineup_valid_success() {
    let channels = host()
        .get_lineup(&tuner("192.168.1.182"))
        .await
        .expect("lineup");
    assert_eq!(channels.len(), 6);
    assert_eq!(channels[0].guide_number.as_deref(), Some("4.1"));
    assert_eq!(channels[0].guide_name.as_deref(), Some("WCMH-DT"));
    assert!(channels[0].hd);
    assert!(channels[0].favorite);
    assert_eq!(
        channels[0].url.as_deref(),
        Some("http://192.168.1.111:5004/auto/v4.1")
    );
}

/// `GetLineup_Legacy_Success` — "Placeholder json is invalid, just need to make
/// sure we can reach it". The legacy fixture's `lineup.json` is `{}`, which is
/// not an array of entries, so upstream throws `JsonException` and so does this.
#[tokio::test]
async fn get_lineup_legacy_success() {
    let err = host()
        .get_lineup(&tuner("10.10.10.100"))
        .await
        .expect_err("`{}` is not a lineup array");
    assert!(err.to_string().contains("lineup.json"), "{err}");
}

/// `GetLineup_ImportFavoritesOnly_Success`.
#[tokio::test]
async fn get_lineup_import_favorites_only_success() {
    let info = TunerHostInfo {
        url: Some("192.168.1.182".to_owned()),
        import_favorites_only: true,
        ..TunerHostInfo::default()
    };
    let channels = host().get_lineup(&info).await.expect("lineup");
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].guide_number.as_deref(), Some("4.1"));
    assert_eq!(channels[0].guide_name.as_deref(), Some("WCMH-DT"));
    assert!(channels[0].hd);
    assert!(channels[0].favorite);
    assert_eq!(
        channels[0].url.as_deref(),
        Some("http://192.168.1.111:5004/auto/v4.1")
    );
}

/// `TryGetTunerHostInfo_Valid_Success`.
#[tokio::test]
async fn try_get_tuner_host_info_valid_success() {
    let host = host();
    let info = host
        .try_get_tuner_host_info("192.168.1.182")
        .await
        .expect("tuner host info");
    assert_eq!(info.type_.as_deref(), Some(host.type_id()));
    assert_eq!(info.url.as_deref(), Some("192.168.1.182"));
    assert_eq!(info.friendly_name.as_deref(), Some("HDHomeRun PRIME"));
    assert_eq!(info.device_id.as_deref(), Some("FFFFFFFF"));
    assert_eq!(info.tuner_count, 3);
}

// ---------------------------------------------------------------- lineup → channels

/// `GetChannelsInternal` — the `ChannelInfo` the lineup turns into. Not one of
/// upstream's facts (it has no test for this method), but it pins the channel
/// IDENTITY, which is the input to the item GUID and must never move.
#[tokio::test]
async fn the_lineup_becomes_hdhr_prefixed_channels() {
    let channels = host()
        .get_channels(&tuner("192.168.1.182"))
        .await
        .expect("channels");
    assert_eq!(channels.len(), 6);
    // `ChannelIdPrefix + i.GuideNumber` with the OVERRIDDEN prefix.
    assert_eq!(channels[0].external_id, "hdhr_4.1");
    assert_eq!(channels[0].name, "WCMH-DT");
    assert_eq!(channels[0].number.as_deref(), Some("4.1"));
    assert_eq!(channels[0].is_hd, Some(true));
    assert!(!channels[0].is_radio, "ChannelType.TV unconditionally");
    // `HD` is absent on 4.2, so the flag is false — not "unknown".
    assert_eq!(channels[1].external_id, "hdhr_4.2");
    assert_eq!(channels[1].is_hd, Some(false));
}

/// `Validate` adopts the device id, and the DRM filter drops encrypted rows.
#[tokio::test]
async fn validate_adopts_the_device_id() {
    let mut info = tuner("192.168.1.182");
    host().validate(&mut info).await.expect("validate");
    assert_eq!(info.device_id.as_deref(), Some("FFFFFFFF"));
}

// ---------------------------------------------------------------- HDHR4 404 fallback

/// Serves `status`/`body` to every request, so the status-dependent branch of
/// `GetModelInfo` can be driven through the REAL fetcher.
async fn serve_once(status: &'static str, body: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0_u8; 1024];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    format!("http://{addr}")
}

/// An HDHR4 has no `discover.json`. A 404 with `throwAllExceptions: false` is
/// answered with the synthetic `{ModelNumber: "HDHR"}` and CACHED, so the miss
/// is paid once; with `throwAllExceptions: true` it is an error, which is what
/// makes `Validate` swallow it explicitly rather than by accident.
#[tokio::test]
async fn hdhr4_404_falls_back_to_a_synthetic_model_and_caches_it() {
    let url = serve_once("404 Not Found", "").await;
    let host = HdHomerunHost::new(Arc::new(ReqwestFetcher::new()));
    let info = TunerHostInfo {
        id: Some("tuner-1".to_owned()),
        url: Some(url.clone()),
        ..TunerHostInfo::default()
    };

    let model = host.get_model_info(&info, false).await.expect("fallback");
    assert_eq!(model.model_number.as_deref(), Some("HDHR"));
    assert!(!model.supports_transcoding());

    // Cached: the second call returns the same synthetic model even with
    // `throwAllExceptions: true`, because the cache is checked first.
    let cached = host.get_model_info(&info, true).await.expect("cached");
    assert_eq!(cached.model_number.as_deref(), Some("HDHR"));

    // A host that never cached it does propagate the 404 when asked to.
    let strict = HdHomerunHost::new(Arc::new(ReqwestFetcher::new()));
    assert!(strict.get_model_info(&info, true).await.is_err());
    // …and `Validate` swallows exactly that error, as upstream does.
    let mut probe = info.clone();
    HdHomerunHost::new(Arc::new(ReqwestFetcher::new()))
        .validate(&mut probe)
        .await
        .expect("a 404 is the HDHR4 case, not a validation failure");
    assert_eq!(probe.device_id, None);
}

/// Any OTHER non-success status is a real failure, both ways.
#[tokio::test]
async fn a_non_404_failure_is_never_swallowed() {
    let url = serve_once("500 Internal Server Error", "").await;
    let info = TunerHostInfo {
        url: Some(url),
        ..TunerHostInfo::default()
    };
    let host = HdHomerunHost::new(Arc::new(ReqwestFetcher::new()));
    assert!(host.get_model_info(&info, false).await.is_err());
    let mut probe = info.clone();
    assert!(host.validate(&mut probe).await.is_err());
}

/// `Validate` clears the cache first (upstream's `lock (_modelCache) { Clear(); }`),
/// so re-saving a tuner whose device changed does not keep the stale model.
#[tokio::test]
async fn validate_clears_the_model_cache_before_re_reading() {
    let host = host();
    let mut info = tuner("192.168.1.182");
    info.id = Some("tuner-1".to_owned());
    host.validate(&mut info).await.expect("validate");
    assert_eq!(info.device_id.as_deref(), Some("FFFFFFFF"));

    // Point the same tuner id at the other device: a cache that was not
    // cleared would still report the PRIME's id.
    info.url = Some("10.10.10.100".to_owned());
    host.validate(&mut info).await.expect("validate");
    assert_eq!(info.device_id.as_deref(), Some("10xxxxx5"));
}
