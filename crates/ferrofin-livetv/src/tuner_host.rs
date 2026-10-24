//! The tuner-host seam.
//!
//! Port of `ITunerHost`/`IConfigurableTunerHost`/`BaseTunerHost` (v10.11.8
//! `src/Jellyfin.LiveTv/TunerHosts/`). Upstream registers one implementation
//! per tuner *kind* in DI (`LiveTvServiceCollectionExtensions.cs:41-42`
//! registers `HdHomerunHost` then `M3UTunerHost`) and `TunerHostManager`
//! projects, dispatches and validates over that collection: the advertised
//! type list is `_tunerHosts.OrderBy(i => i.Name)`, saving a host looks its
//! `Type` up in the same collection and 404s when nothing matches, and a guide
//! refresh asks each configured host for its own lineup.
//!
//! Ferrofin holds the same collection as `Vec<Arc<dyn TunerHost>>` on
//! [`FerrofinLiveTvManager`](crate::FerrofinLiveTvManager). Everything a host
//! must answer — its identity, its lineup, its media sources and the stream to
//! open — goes through this trait, so adding a tuner kind is adding an
//! implementation and registering it, never another `match` in the manager.

use async_trait::async_trait;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::live_tv::TunerHostInfo;
use ferrofin_traits::error::ServiceError;

use crate::fetch::SourceFetcher;
use crate::m3u::{M3uParser, is_valid_channel_url};

/// One channel as a tuner reported it, before anything is persisted.
///
/// Port of the `ChannelInfo` fields a tuner host fills in
/// (`MediaBrowser.Controller/LiveTv/ChannelInfo.cs`); the ones Ferrofin has no
/// column for (`CallSign`, `HasImage`, …) are not carried because nothing
/// downstream reads them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TunerChannel {
    /// `ChannelInfo.Id` — the tuner-facing external id, and the only input to
    /// the channel's internal GUID. A pure function of the tuner and the
    /// lineup entry, never of this server instance.
    pub external_id: String,
    /// The key the XMLTV guide joins on.
    ///
    /// `ListingsManager.GetEpgChannelFromTunerChannel` (v10.11.8
    /// ListingsManager.cs:353-400) tries `ChannelInfo.Id`, then
    /// `TunerChannelId`, then `Number`. Ferrofin stores the one key that can
    /// actually match — `tvg-id` for M3U (which is upstream's
    /// `TunerChannelId`), the guide number for HDHomeRun (whose `ChannelInfo`
    /// leaves `TunerChannelId` unset, so `Number` is the leg that fires).
    pub guide_key: String,
    /// `ChannelInfo.Name`.
    pub name: String,
    /// `ChannelInfo.Number`.
    pub number: Option<String>,
    /// `ChannelInfo.ImageUrl`.
    pub image_url: Option<String>,
    /// `ChannelInfo.ChannelType == ChannelType.Radio`.
    pub is_radio: bool,
    /// `ChannelInfo.Path` — the URL that plays the channel.
    pub url: String,
    /// `ChannelInfo.IsHD`. `None` is "the tuner did not say", which
    /// `HdHomerunHost.GetMediaSource` reads as `?? true`.
    pub is_hd: Option<bool>,
    /// `ChannelInfo.VideoCodec`.
    pub video_codec: Option<String>,
    /// `ChannelInfo.AudioCodec`.
    pub audio_codec: Option<String>,
}

/// A stored channel, as the media-source and stream paths see it.
///
/// The persisted half of [`TunerChannel`]: what `channel_media_sources` and
/// `channel_stream` need in order to rebuild upstream's `ChannelInfo` without
/// re-fetching the lineup.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredChannel {
    /// `ChannelInfo.Id`.
    pub external_id: String,
    /// `ChannelInfo.Path`.
    pub path: String,
    /// `ChannelInfo.IsHD`.
    pub is_hd: Option<bool>,
    /// `ChannelInfo.VideoCodec`.
    pub video_codec: Option<String>,
    /// `ChannelInfo.AudioCodec`.
    pub audio_codec: Option<String>,
}

/// How the manager must open the stream a host chose.
///
/// The two arms are upstream's two `ILiveStream` implementations: every host
/// but the legacy HDHomeRun control path returns a `SharedHttpStream`, and the
/// legacy path returns an `HdHomerunUdpStream`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelStreamKind {
    /// Open `MediaSourceInfo.Path` over HTTP and share the buffered copy
    /// (`SharedHttpStream`).
    Http,
    /// Drive the device's UDP control protocol and receive RTP
    /// (`HdHomerunUdpStream`).
    LegacyUdp(Box<crate::hdhomerun::LegacyUdpPlan>),
}

/// The stream one host chose for one channel: the media source to hand back
/// and how to open it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelStream {
    /// The media source, already carrying the path to open.
    pub source: MediaSourceInfo,
    /// Which of upstream's two live-stream shapes this is.
    pub kind: ChannelStreamKind,
}

/// A tuner backend: one implementation per tuner *kind*.
///
/// Port of `ITunerHost` + `IConfigurableTunerHost` + the `BaseTunerHost`
/// defaults. Object-safe by construction so the manager can hold
/// `Vec<Arc<dyn TunerHost>>`, exactly as upstream holds `IEnumerable<ITunerHost>`.
#[async_trait]
pub trait TunerHost: Send + Sync {
    /// `ITunerHost.Name` — the display name `GET /LiveTv/TunerHosts/Types`
    /// advertises and orders by.
    fn name(&self) -> &'static str;

    /// `ITunerHost.Type` — the stable id a `TunerHostInfo.Type` names.
    fn type_id(&self) -> &'static str;

    /// `BaseTunerHost.ChannelIdPrefix`, whose default is `Type + "_"`.
    fn channel_id_prefix(&self) -> String {
        format!("{}_", self.type_id())
    }

    /// `BaseTunerHost.IsSupported` — `true` unless an override says otherwise.
    /// `TunerHostManager` filters the collection on it before anything else,
    /// so an unsupported host is neither advertised nor saveable.
    fn is_supported(&self) -> bool {
        true
    }

    /// `BaseTunerHost.GetChannelsInternal` — this host's lineup for one
    /// configured tuner.
    async fn get_channels(&self, tuner: &TunerHostInfo) -> Result<Vec<TunerChannel>, ServiceError>;

    /// `IConfigurableTunerHost.Validate` — reject (or enrich) a tuner host
    /// before it is saved. The default accepts anything, which is what a host
    /// that does not implement `IConfigurableTunerHost` does.
    async fn validate(&self, _info: &mut TunerHostInfo) -> Result<(), ServiceError> {
        Ok(())
    }

    /// `ITunerHost.DiscoverDevices` — devices of this kind reachable on the
    /// network within `duration_ms`. The default finds none, which is what a
    /// host with no discovery protocol reports.
    async fn discover_devices(
        &self,
        _duration_ms: u64,
    ) -> Result<Vec<TunerHostInfo>, ServiceError> {
        Ok(Vec::new())
    }

    /// `BaseTunerHost.GetChannelStreamMediaSources` — the unopened media
    /// sources this channel offers.
    async fn channel_media_sources(
        &self,
        tuner: &TunerHostInfo,
        channel: &StoredChannel,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError>;

    /// `BaseTunerHost.GetChannelStream` — the stream to open for `stream_id`
    /// (the media-source id a client picked, or `None` for the default).
    async fn channel_stream(
        &self,
        tuner: &TunerHostInfo,
        channel: &StoredChannel,
        stream_id: Option<&str>,
    ) -> Result<ChannelStream, ServiceError>;
}

/// Compile-time proof the trait stays usable as `Arc<dyn TunerHost>`.
fn _assert_object_safe(_: &dyn TunerHost) {}

/// The `m3u` tuner's channel-id prefix, before the tuner-URL hash.
///
/// Port of `BaseTunerHost.ChannelIdPrefix` => `Type + "_"` with
/// `M3UTunerHost.Type == "m3u"` (v10.11.8 BaseTunerHost.cs:46,
/// M3UTunerHost.cs:60).
pub(crate) const M3U_CHANNEL_ID_PREFIX: &str = "m3u_";

/// The tuner-facing external id of one M3U channel.
///
/// Port of `M3UTunerHost.GetFullChannelIdPrefix` (v10.11.8 M3UTunerHost.cs:64-67)
/// followed by `M3uParser.GetChannelsAsync` (M3uParser.cs:104), which
/// *overwrites* the `tvg-id`-derived id set in `GetChannelInfo` with
/// `prefix + MD5(streamUrlLine)`. The identity of a channel upstream is
/// therefore `(tuner URL, stream URL)` — never the `tvg-id`, which survives only
/// as the guide join key (`TunerChannelId`). Two playlist entries sharing a
/// `tvg-id` stay distinct here, exactly as they do upstream.
pub(crate) fn m3u_external_channel_id(tuner_url: &str, stream_url: &str) -> String {
    format!(
        "{M3U_CHANNEL_ID_PREFIX}{}{}",
        ferrofin_common::extensions::get_md5(tuner_url).simple(),
        ferrofin_common::extensions::get_md5(stream_url).simple()
    )
}

/// The M3U playlist tuner.
///
/// Port of `M3UTunerHost` (v10.11.8 `src/Jellyfin.LiveTv/TunerHosts/M3UTunerHost.cs`)
/// over the existing [`parse_m3u`] port: the lineup is the playlist, and every
/// stream is a plain HTTP one.
pub struct M3uTunerHost {
    /// The HTTP/file seam the playlist is read through.
    fetcher: std::sync::Arc<dyn SourceFetcher>,
    /// The server's local-network check, shared with the manager that sets it
    /// once the network policy is wired; empty in unit tests.
    local_network: std::sync::Arc<std::sync::OnceLock<crate::stream::LocalNetworkCheck>>,
}

impl std::fmt::Debug for M3uTunerHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("M3uTunerHost").finish_non_exhaustive()
    }
}

impl M3uTunerHost {
    /// Creates the host over the given source fetcher.
    #[must_use]
    pub fn new(fetcher: std::sync::Arc<dyn SourceFetcher>) -> Self {
        Self {
            fetcher,
            local_network: std::sync::Arc::default(),
        }
    }

    /// Reads the server's local-network check from `local_network` once it
    /// is set; until then (unit tests) the stateless
    /// [`is_in_local_network`](crate::stream::is_in_local_network) applies.
    #[must_use]
    pub fn sharing_local_network(
        mut self,
        local_network: std::sync::Arc<std::sync::OnceLock<crate::stream::LocalNetworkCheck>>,
    ) -> Self {
        self.local_network = local_network;
        self
    }

    /// `M3UTunerHost.CreateMediaSourceInfo`'s `isRemote`: a source is remote
    /// unless its URL's host is on the local network
    /// (`!_networkManager.IsInLocalNetwork(uri.Host)`). The check resolves a
    /// host name, which can block on DNS, so it runs off the async workers.
    async fn is_remote(&self, path: &str) -> bool {
        let Some(host) = crate::stream::url_host(path) else {
            return true;
        };
        match self.local_network.get() {
            Some(check) => {
                let (check, host) = (std::sync::Arc::clone(check), host.to_owned());
                tokio::task::spawn_blocking(move || !check(&host))
                    .await
                    .unwrap_or(true)
            }
            None => !crate::stream::is_in_local_network(host),
        }
    }
}

#[async_trait]
impl TunerHost for M3uTunerHost {
    fn name(&self) -> &'static str {
        // `M3UTunerHost.Name` (v10.11.8 M3UTunerHost.cs:58).
        "M3U Tuner"
    }

    fn type_id(&self) -> &'static str {
        // `M3UTunerHost.Type` (v10.11.8 M3UTunerHost.cs:60).
        "m3u"
    }

    async fn get_channels(&self, tuner: &TunerHostInfo) -> Result<Vec<TunerChannel>, ServiceError> {
        let url = tuner.url.as_deref().unwrap_or_default();
        // `M3uParser.GetListingsStream` presents the tuner's user agent and
        // fails on a non-success status (`EnsureSuccessStatusCode`); `Parse`
        // then reads the playlist line by line as it arrives (v12.2
        // M3uParser.cs:39-70), never holding it whole. Like upstream, an
        // endless body is read for ever, and one that never ends a line
        // grows that line.
        let mut opened = self.fetcher.open(url, tuner.user_agent.as_deref()).await?;
        if !opened.is_success() {
            return Err(ServiceError::backend(format!(
                "fetch {url} answered {}",
                opened.status
            )));
        }
        let mut parser = M3uParser::new();
        while let Some(chunk) = opened.body.next_chunk().await? {
            parser.push(&chunk);
        }
        Ok(parser
            .finish()
            .into_iter()
            .map(|ch| TunerChannel {
                external_id: m3u_external_channel_id(url, &ch.url),
                guide_key: ch.id,
                name: ch.name,
                number: ch.number,
                image_url: ch.logo,
                is_radio: ch.is_radio,
                url: ch.url,
                // The playlist carries no per-channel media facts, so the M3U
                // host reports none — `M3uParser` sets neither `IsHD` nor a
                // codec, and `M3UTunerHost.CreateMediaSourceInfo` reads none.
                is_hd: None,
                video_codec: None,
                audio_codec: None,
            })
            .collect())
    }

    async fn channel_media_sources(
        &self,
        tuner: &TunerHostInfo,
        channel: &StoredChannel,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        // `M3UTunerHost.GetChannelStreamMediaSources` returns the ONE source
        // `CreateMediaSourceInfo` builds from the playlist URL.
        let is_remote = self.is_remote(&channel.path).await;
        Ok(vec![m3u_channel_source(tuner, channel, is_remote)?])
    }

    async fn channel_stream(
        &self,
        tuner: &TunerHostInfo,
        channel: &StoredChannel,
        _stream_id: Option<&str>,
    ) -> Result<ChannelStream, ServiceError> {
        // `M3UTunerHost.GetChannelStream` ignores the stream id: the playlist
        // offers exactly one source per channel.
        let is_remote = self.is_remote(&channel.path).await;
        Ok(ChannelStream {
            source: m3u_channel_source(tuner, channel, is_remote)?,
            kind: ChannelStreamKind::Http,
        })
    }
}

/// The one source an M3U channel plays from (`M3UTunerHost.CreateMediaSourceInfo`).
///
/// A stored path that [`is_valid_channel_url`] rejects is refused. Upstream
/// re-parses the playlist after a restart, so its parser check covers every
/// channel it serves. Channels persist here, and a row saved before the check
/// existed stays until the next guide refresh drops it. Serving it would hand
/// its path to the anonymous stream routes as a file.
fn m3u_channel_source(
    tuner: &TunerHostInfo,
    channel: &StoredChannel,
    is_remote: bool,
) -> Result<MediaSourceInfo, ServiceError> {
    if !is_valid_channel_url(&channel.path) {
        return Err(ServiceError::not_found(format!(
            "playable URL for M3U channel {}",
            channel.external_id
        )));
    }
    let mut source = crate::stream::create_media_source_info(&channel.path, tuner, is_remote);
    crate::stream::normalize(&mut source);
    Ok(source)
}

/// The advertised tuner-host types, ordered by name.
///
/// Port of `TunerHostManager.GetTunerHostTypes` (v10.11.8
/// TunerHostManager.cs:52-58) over the `IsSupported`-filtered collection the
/// constructor built (:44): `_tunerHosts.OrderBy(i => i.Name).Select(i => new
/// NameIdPair { Name = i.Name, Id = i.Type })`.
#[must_use]
pub fn tuner_host_types(
    hosts: &[std::sync::Arc<dyn TunerHost>],
) -> Vec<ferrofin_model::dto::NameIdPair> {
    let mut supported: Vec<_> = hosts.iter().filter(|h| h.is_supported()).collect();
    // `OrderBy` is a STABLE sort on the ordinal string comparison .NET's
    // default `Comparer<string>` uses for `OrderBy(i => i.Name)` under the
    // invariant culture the server runs in.
    supported.sort_by(|a, b| a.name().cmp(b.name()));
    supported
        .into_iter()
        .map(|h| ferrofin_model::dto::NameIdPair {
            name: Some(h.name().to_owned()),
            id: Some(h.type_id().to_owned()),
        })
        .collect()
}

/// The registered host whose `Type` matches `type_id`, case-insensitively.
///
/// Port of `TunerHostManager.SaveTunerHost`'s lookup (v10.11.8
/// TunerHostManager.cs:63): `_tunerHosts.FirstOrDefault(i =>
/// string.Equals(info.Type, i.Type, StringComparison.OrdinalIgnoreCase))`,
/// over the same `IsSupported`-filtered collection.
#[must_use]
pub fn find_host<'a>(
    hosts: &'a [std::sync::Arc<dyn TunerHost>],
    type_id: &str,
) -> Option<&'a std::sync::Arc<dyn TunerHost>> {
    hosts
        .iter()
        .find(|h| h.is_supported() && h.type_id().eq_ignore_ascii_case(type_id))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        ChannelStreamKind, M3uTunerHost, StoredChannel, TunerHost, find_host,
        m3u_external_channel_id, tuner_host_types,
    };
    use crate::fetch::{SourceBody, SourceFetcher};
    use ferrofin_model::live_tv::TunerHostInfo;
    use ferrofin_traits::error::ServiceError;

    /// A fetcher that serves one canned body for every URL.
    struct Canned(String);

    #[async_trait::async_trait]
    impl SourceFetcher for Canned {
        async fn fetch(&self, _url: &str) -> Result<String, ServiceError> {
            Ok(self.0.clone())
        }
    }

    fn m3u_host(body: &str) -> M3uTunerHost {
        M3uTunerHost::new(Arc::new(Canned(body.to_owned())))
    }

    /// The M3U host fetches the playlist as the tuner's configured user agent.
    #[tokio::test]
    async fn the_m3u_host_fetches_with_the_tuners_user_agent() {
        struct Recording(std::sync::Mutex<Vec<Option<String>>>);

        #[async_trait::async_trait]
        impl SourceFetcher for Recording {
            async fn fetch(&self, _url: &str) -> Result<String, ServiceError> {
                Ok(String::new())
            }
            async fn open(
                &self,
                _url: &str,
                user_agent: Option<&str>,
            ) -> Result<SourceBody, ServiceError> {
                self.0.lock().unwrap().push(user_agent.map(str::to_owned));
                Ok(SourceBody::from_bytes(200, Vec::new()))
            }
        }
        let fetcher = Arc::new(Recording(std::sync::Mutex::new(Vec::new())));
        let host = M3uTunerHost::new(fetcher.clone());
        for user_agent in [Some("VLC/3.0".to_owned()), None] {
            let tuner = TunerHostInfo {
                url: Some("http://iptv.example/list.m3u".to_owned()),
                user_agent,
                ..TunerHostInfo::default()
            };
            host.get_channels(&tuner).await.expect("lineup");
        }
        assert_eq!(
            *fetcher.0.lock().unwrap(),
            [Some("VLC/3.0".to_owned()), None]
        );
    }

    /// `EnsureSuccessStatusCode` (v12.2 M3uParser.cs:67): a playlist URL that
    /// answers a failure status is an error, whatever its body says, not a
    /// lineup.
    #[tokio::test]
    async fn a_failure_status_is_an_error_not_a_lineup() {
        struct NotFound;

        #[async_trait::async_trait]
        impl SourceFetcher for NotFound {
            async fn fetch(&self, _url: &str) -> Result<String, ServiceError> {
                Ok(String::new())
            }
            async fn open(
                &self,
                _url: &str,
                _user_agent: Option<&str>,
            ) -> Result<SourceBody, ServiceError> {
                Ok(SourceBody::from_bytes(
                    404,
                    b"#EXTINF:-1,Error page\nhttp://iptv.example/404\n".to_vec(),
                ))
            }
        }
        let tuner = TunerHostInfo {
            url: Some("http://iptv.example/list.m3u".to_owned()),
            ..TunerHostInfo::default()
        };
        let err = M3uTunerHost::new(Arc::new(NotFound))
            .get_channels(&tuner)
            .await
            .expect_err("a 404 is no playlist");
        assert!(
            matches!(&err, ServiceError::Backend(why)
                if why == "fetch http://iptv.example/list.m3u answered 404"),
            "{err:?}"
        );
    }

    /// Where a playlist test serves its bytes from.
    #[derive(Debug, Clone, Copy)]
    enum Source {
        /// A local file (`AsyncFile.OpenRead`).
        File,
        /// A one-shot HTTP peer (`ReadAsStreamAsync`).
        Http,
    }

    /// Serves `body` from `source` and answers the URL a tuner points at; the
    /// directory keeps a file alive for the test.
    fn serve_playlist(source: Source, body: Vec<u8>) -> (String, tempfile::TempDir) {
        use std::io::{Read as _, Write as _};
        let dir = tempfile::tempdir().expect("tmp");
        match source {
            Source::File => {
                let path = dir.path().join("list.m3u");
                std::fs::write(&path, body).expect("write");
                (path.to_str().expect("utf-8 path").to_owned(), dir)
            }
            Source::Http => {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
                let url = format!("http://{}/list.m3u", listener.local_addr().expect("addr"));
                std::thread::spawn(move || {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let mut buffer = [0_u8; 4096];
                        let _ = stream.read(&mut buffer);
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                    }
                });
                (url, dir)
            }
        }
    }

    /// `text` as UTF-16 in `unit`'s byte order behind its byte-order mark.
    fn utf16(text: &str, bom: [u8; 2], unit: fn(u16) -> [u8; 2]) -> Vec<u8> {
        let mut bytes = bom.to_vec();
        bytes.extend(text.encode_utf16().flat_map(unit));
        bytes
    }

    /// `M3uParser.Parse` reads the playlist through `new StreamReader(stream)`
    /// and `ReadLineAsync` (v12.2 M3uParser.cs:42-45,77): UTF-8 unless a
    /// byte-order mark names UTF-16 or UTF-32, the mark dropped, an invalid
    /// byte read as U+FFFD, and a line ended by `\n`, `\r` or `\r\n` — from a
    /// file and over HTTP alike.
    #[rstest::rstest]
    #[case::cr_only(
        b"#EXTM3U\r#EXTINF:-1,One\rhttp://tuner/1\r#EXTINF:-1,Two\rhttp://tuner/2\r".to_vec(),
        &["One", "Two"]
    )]
    #[case::mixed_endings(
        b"#EXTM3U\r\n#EXTINF:-1,One\rhttp://tuner/1\n\n#EXTINF:-1,Two\r\nhttp://tuner/2".to_vec(),
        &["One", "Two"]
    )]
    #[case::utf8_bom(b"\xEF\xBB\xBF#EXTINF:-1,One\nhttp://tuner/1\n".to_vec(), &["One"])]
    #[case::latin1(b"#EXTINF:-1,Caf\xE9\nhttp://tuner/1\n".to_vec(), &["Caf\u{FFFD}"])]
    #[case::utf16_le(
        utf16("#EXTINF:-1,Café\r\nhttp://tuner/1\r\n", [0xFF, 0xFE], u16::to_le_bytes),
        &["Café"]
    )]
    #[case::utf16_be(
        utf16("#EXTINF:-1,Café\nhttp://tuner/1", [0xFE, 0xFF], u16::to_be_bytes),
        &["Café"]
    )]
    #[tokio::test]
    async fn the_playlist_is_read_like_a_dotnet_stream_reader(
        #[case] body: Vec<u8>,
        #[case] names: &[&str],
        #[values(Source::File, Source::Http)] source: Source,
    ) {
        let (url, _dir) = serve_playlist(source, body);
        let host = M3uTunerHost::new(Arc::new(crate::fetch::ReqwestFetcher::new()));
        let tuner = TunerHostInfo {
            url: Some(url),
            ..TunerHostInfo::default()
        };
        let channels = host.get_channels(&tuner).await.expect("lineup");
        assert_eq!(
            channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            names,
            "{source:?}"
        );
    }

    #[test]
    fn the_m3u_host_reports_its_upstream_identity() {
        let host = m3u_host("");
        assert_eq!(host.name(), "M3U Tuner");
        assert_eq!(host.type_id(), "m3u");
        // `BaseTunerHost.ChannelIdPrefix` default.
        assert_eq!(host.channel_id_prefix(), "m3u_");
        assert!(host.is_supported());
    }

    #[tokio::test]
    async fn the_m3u_lineup_keeps_the_committed_external_id() {
        // The identity of an M3U channel must not move: it is the input to the
        // item GUID, so a change here re-creates every channel item.
        let host = m3u_host(
            "#EXTINF:-1 tvg-id=\"BBCOne.uk\" tvg-chno=\"1\",BBC One\nhttp://tuner/bbc1.ts\n",
        );
        let tuner = TunerHostInfo {
            url: Some("http://tuner/list.m3u".to_owned()),
            ..TunerHostInfo::default()
        };
        let channels = host.get_channels(&tuner).await.expect("lineup");
        assert_eq!(channels.len(), 1);
        assert_eq!(
            channels[0].external_id,
            m3u_external_channel_id("http://tuner/list.m3u", "http://tuner/bbc1.ts")
        );
        assert_eq!(channels[0].guide_key, "BBCOne.uk");
        assert_eq!(channels[0].number.as_deref(), Some("1"));
        // The playlist says nothing about the media, so neither does the host.
        assert_eq!(channels[0].is_hd, None);
        assert_eq!(channels[0].video_codec, None);
    }

    /// `IsInLocalNetwork(uri.Host)` is the server's network policy, which
    /// resolves host names: a tuner addressed by name on the LAN is local,
    /// one the policy calls remote is remote, and with no policy wired an
    /// IPv4-mapped literal counts as the private IPv4 address it is.
    #[tokio::test]
    async fn an_m3u_source_is_remote_as_the_network_policy_says() {
        let tuner = TunerHostInfo::default();
        let channel = |path: &str| StoredChannel {
            external_id: "m3u_abc".to_owned(),
            path: path.to_owned(),
            ..StoredChannel::default()
        };
        let policy = Arc::new(std::sync::OnceLock::new());
        let check: crate::stream::LocalNetworkCheck =
            Arc::new(|host: &str| host == "tvheadend.lan");
        assert!(policy.set(check).is_ok());
        let host = m3u_host("").sharing_local_network(Arc::clone(&policy));
        for (path, remote) in [
            ("http://tvheadend.lan:9981/stream/channel/1", false),
            ("http://tv.example.com/bbc1.ts", true),
            ("http://192.168.1.20/bbc1.ts", true),
        ] {
            let sources = host
                .channel_media_sources(&tuner, &channel(path))
                .await
                .expect("sources");
            assert_eq!(sources[0].is_remote, remote, "{path}");
            let stream = host
                .channel_stream(&tuner, &channel(path), None)
                .await
                .expect("stream");
            assert_eq!(stream.source.is_remote, remote, "{path}");
        }
        let unwired = m3u_host("");
        for (path, remote) in [
            ("http://[::ffff:192.168.1.5]:8080/bbc1.ts", false),
            ("http://192.168.1.20/bbc1.ts", false),
            ("http://tvheadend.lan/bbc1.ts", true),
        ] {
            let sources = unwired
                .channel_media_sources(&tuner, &channel(path))
                .await
                .expect("sources");
            assert_eq!(sources[0].is_remote, remote, "no policy: {path}");
        }
    }

    #[tokio::test]
    async fn the_m3u_stream_is_always_a_plain_http_one() {
        let host = m3u_host("");
        let tuner = TunerHostInfo::default();
        let channel = StoredChannel {
            external_id: "m3u_abc".to_owned(),
            path: "http://tuner/bbc1.ts".to_owned(),
            ..StoredChannel::default()
        };
        let sources = host
            .channel_media_sources(&tuner, &channel)
            .await
            .expect("sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path.as_deref(), Some("http://tuner/bbc1.ts"));
        let stream = host
            .channel_stream(&tuner, &channel, Some("whatever"))
            .await
            .expect("stream");
        assert_eq!(stream.kind, ChannelStreamKind::Http);
        assert_eq!(stream.source.path.as_deref(), Some("http://tuner/bbc1.ts"));
    }

    /// A channel stored before the parser rejected file paths is never served.
    #[tokio::test]
    async fn a_stored_m3u_channel_with_a_file_path_has_no_source() {
        let host = m3u_host("");
        let tuner = TunerHostInfo::default();
        let channel = StoredChannel {
            external_id: "m3u_abc".to_owned(),
            path: "/var/lib/ferrofin/jellyfin.db".to_owned(),
            ..StoredChannel::default()
        };
        let err = host
            .channel_media_sources(&tuner, &channel)
            .await
            .expect_err("a file path is refused");
        assert!(matches!(err, ServiceError::NotFound(_)), "{err:?}");
        assert!(
            host.channel_stream(&tuner, &channel, None).await.is_err(),
            "opening it is refused too"
        );
    }

    /// A host that reports itself unsupported, to prove the filter runs.
    struct Unsupported;

    #[async_trait::async_trait]
    impl TunerHost for Unsupported {
        fn name(&self) -> &'static str {
            "AAA Unsupported"
        }
        fn type_id(&self) -> &'static str {
            "unsupported"
        }
        fn is_supported(&self) -> bool {
            false
        }
        async fn get_channels(
            &self,
            _tuner: &TunerHostInfo,
        ) -> Result<Vec<super::TunerChannel>, ServiceError> {
            Ok(Vec::new())
        }
        async fn channel_media_sources(
            &self,
            _tuner: &TunerHostInfo,
            _channel: &StoredChannel,
        ) -> Result<Vec<ferrofin_model::dto::MediaSourceInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn channel_stream(
            &self,
            _tuner: &TunerHostInfo,
            _channel: &StoredChannel,
            _stream_id: Option<&str>,
        ) -> Result<super::ChannelStream, ServiceError> {
            Err(ServiceError::backend("no"))
        }
    }

    #[test]
    fn the_type_list_is_name_ordered_and_skips_unsupported_hosts() {
        let hosts: Vec<Arc<dyn TunerHost>> = vec![
            Arc::new(m3u_host("")),
            Arc::new(crate::hdhomerun::HdHomerunHost::new(Arc::new(Canned(
                String::new(),
            )))),
            Arc::new(Unsupported),
        ];
        let types = tuner_host_types(&hosts);
        // "HD Homerun" < "M3U Tuner" ordinally, and the unsupported host —
        // which sorts FIRST by name — is filtered out before the ordering.
        assert_eq!(
            types
                .iter()
                .map(|p| (p.name.clone().unwrap(), p.id.clone().unwrap()))
                .collect::<Vec<_>>(),
            vec![
                ("HD Homerun".to_owned(), "hdhomerun".to_owned()),
                ("M3U Tuner".to_owned(), "m3u".to_owned()),
            ]
        );
    }

    #[test]
    fn the_host_lookup_is_case_insensitive_and_skips_unsupported() {
        let hosts: Vec<Arc<dyn TunerHost>> = vec![Arc::new(m3u_host("")), Arc::new(Unsupported)];
        assert_eq!(find_host(&hosts, "M3U").map(|h| h.type_id()), Some("m3u"));
        assert!(find_host(&hosts, "unsupported").is_none());
        assert!(find_host(&hosts, "nosuchtype").is_none());
    }
}
