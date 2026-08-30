//! The HDHomeRun tuner host.
//!
//! Port of `HdHomerunHost.cs` (v10.11.8
//! `src/Jellyfin.LiveTv/TunerHosts/HdHomerun/HdHomerunHost.cs`, 557 lines).
//!
//! A SiliconDust HDHomeRun exposes itself over three plain HTTP documents —
//! `discover.json`, `lineup.json` and a per-channel `.../auto/vN.N` MPEG-TS
//! stream — plus a UDP broadcast that answers a 20-byte discovery datagram on
//! port 65001. Everything this host does is one of those, which is why the
//! whole surface is verifiable without the device: upstream's own oracle
//! (`tests/Jellyfin.LiveTv.Tests/HdHomerunHostTests.cs`) mocks the HTTP handler
//! and reads the same JSON fixtures this module's tests read.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::live_tv::TunerHostInfo;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::error::ServiceError;

use crate::error::LiveTvError;
use crate::fetch::SourceFetcher;
use crate::hdhomerun::types::{DiscoverResponse, LineupChannel};
use crate::tuner_host::{ChannelStream, ChannelStreamKind, StoredChannel, TunerChannel, TunerHost};

/// `HdHomerunHost.ChannelIdPrefix` (v10.11.8 HdHomerunHost.cs:69).
pub const HDHR_CHANNEL_ID_PREFIX: &str = "hdhr_";

/// The model number the HDHR4 fallback stands in with.
///
/// `HdHomerunHost.GetModelInfo`'s `const string DefaultValue = "HDHR"`
/// (v10.11.8 HdHomerunHost.cs:139): an HDHR4 has no `discover.json`, so a 404
/// is answered with a synthetic response whose only field is this, and which is
/// cached so the miss is paid once.
const HDHR4_FALLBACK_MODEL: &str = "HDHR";

/// The 20-byte discovery datagram, verbatim from
/// `HdHomerunHost.DiscoverDevices` (v10.11.8 HdHomerunHost.cs:481):
/// a `HDHOMERUN_TYPE_DISCOVER_REQ` carrying the wildcard device type and
/// device id, and its CRC.
const DISCOVERY_DATAGRAM: [u8; 20] = [
    0, 2, 0, 12, 1, 4, 255, 255, 255, 255, 2, 4, 255, 255, 255, 255, 115, 204, 125, 143,
];

/// `HdHomerunHost.DiscoverDevices`' receive buffer (`new byte[8192]`,
/// HdHomerunHost.cs:489).
const DISCOVERY_BUFFER: usize = 8192;

/// The plan for a legacy (UDP-controlled) HDHomeRun stream.
///
/// What `HdHomerunHost.GetChannelStream` hands to `HdHomerunUdpStream`'s
/// constructor when the lineup entry's URL is a `hdhomerun://` locator
/// (HdHomerunHost.cs:396-407): the device address, the tuner count to walk and
/// the channel commands to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyUdpPlan {
    /// The device's control endpoint: the IP parsed from the media source's
    /// path, on `HdHomerunManager.HdHomeRunPort`.
    ///
    /// A whole `SocketAddr` rather than the bare address upstream carries, so
    /// the control protocol can be driven against a fake device bound to an
    /// ephemeral port — the port is still the constant on every real path.
    pub device: SocketAddr,
    /// `modelInfo.TunerCount` — how many tuners `StartStreaming` may try.
    pub num_tuners: i32,
    /// `LegacyHdHomerunChannelCommands(hdhomerunChannel.Path).GetCommands()`.
    pub commands: Vec<(String, String)>,
}

/// The HDHomeRun tuner host.
///
/// Port of `HdHomerunHost` (v10.11.8 HdHomerunHost.cs). `IsSupported` is not
/// overridden, so — exactly as upstream — the type is advertised whether or not
/// a device is on the network.
pub struct HdHomerunHost {
    /// The HTTP seam `discover.json`/`lineup.json` are read through.
    fetcher: Arc<dyn SourceFetcher>,
    /// `HdHomerunHost._modelCache`, keyed on `TunerHostInfo.Id`
    /// (HdHomerunHost.cs:42). A `std::sync::Mutex` because the guard never
    /// crosses an `.await`, matching upstream's `lock (_modelCache)`.
    model_cache: Mutex<HashMap<String, DiscoverResponse>>,
}

impl std::fmt::Debug for HdHomerunHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HdHomerunHost").finish_non_exhaustive()
    }
}

/// `HdHomerunHost.GetApiUrl` (v10.11.8 HdHomerunHost.cs:161-175).
///
/// An empty/whitespace URL is `ArgumentException`; a URL that does not already
/// start with `http` (case-insensitively) gets `http://` prepended; then
/// `new Uri(url).AbsoluteUri.TrimEnd('/')`.
///
/// `AbsoluteUri` normalizes: it lower-cases the scheme and host and supplies a
/// `/` path when there is none, which is why `"192.168.1.182"` becomes
/// `"http://192.168.1.182"` after the trailing-slash trim.
///
/// # Errors
///
/// [`ServiceError::InvalidInput`] for an absent or blank URL, matching
/// upstream's `ArgumentException("Invalid tuner info")`.
pub fn get_api_url(info: &TunerHostInfo) -> Result<String, ServiceError> {
    let url = info.url.as_deref().unwrap_or_default().trim();
    if url.is_empty() {
        return Err(ServiceError::InvalidInput("Invalid tuner info".to_owned()));
    }
    let url = if url.len() >= 4 && url[..4].eq_ignore_ascii_case("http") {
        url.to_owned()
    } else {
        format!("http://{url}")
    };
    Ok(normalize_absolute_uri(&url)
        .trim_end_matches('/')
        .to_owned())
}

/// `new Uri(url).AbsoluteUri` for the shapes a tuner URL takes: the scheme and
/// the authority fold to lower case, and an absent path becomes `/`.
fn normalize_absolute_uri(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(cut) => (&rest[..cut], &rest[cut..]),
        None => (rest, "/"),
    };
    format!(
        "{}://{}{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase(),
        path
    )
}

/// `HdHomerunHost.GetHdHrIdFromChannelId` (v10.11.8 HdHomerunHost.cs:177-180):
/// `channelId.Split('_')[1]` — the guide number behind the `hdhr_` prefix.
fn hdhr_id_from_channel_id(channel_id: &str) -> &str {
    channel_id.split('_').nth(1).unwrap_or_default()
}

/// One row of `HdHomerunHost.GetMediaSource`'s transcode-profile table
/// (v10.11.8 HdHomerunHost.cs:187-247). These are the device's own advertised
/// profiles — ported constants, not Ferrofin tuning knobs.
struct Profile {
    /// Frame width.
    width: i32,
    /// Frame height.
    height: i32,
    /// The video bit rate the device targets for this profile.
    video_bitrate: i32,
}

/// The profile table, looked up case-insensitively as
/// `string.Equals(profile, "...", StringComparison.OrdinalIgnoreCase)` does.
fn transcode_profile(profile: &str) -> Option<Profile> {
    let row = match profile.to_ascii_lowercase().as_str() {
        "mobile" => (1280, 720, 2_000_000),
        "heavy" => (1920, 1080, 15_000_000),
        "internet720" => (1280, 720, 8_000_000),
        "internet540" => (960, 540, 2_500_000),
        "internet480" => (848, 480, 2_000_000),
        "internet360" => (640, 360, 1_500_000),
        "internet240" => (432, 240, 1_000_000),
        _ => return None,
    };
    Some(Profile {
        width: row.0,
        height: row.1,
        video_bitrate: row.2,
    })
}

/// The transcoding profiles `GetChannelStreamMediaSources` offers, in upstream's
/// order (v10.11.8 HdHomerunHost.cs:357-364), before the trailing `"native"`.
const HW_TRANSCODING_PROFILES: [&str; 6] = [
    "heavy",
    "internet540",
    "internet480",
    "internet360",
    "internet240",
    "mobile",
];

impl HdHomerunHost {
    /// Creates the host over the given HTTP seam.
    #[must_use]
    pub fn new(fetcher: Arc<dyn SourceFetcher>) -> Self {
        Self {
            fetcher,
            model_cache: Mutex::new(HashMap::new()),
        }
    }

    /// The model cache guard, taking the lock back after a poisoning panic —
    /// a cached `DiscoverResponse` is derived data, so a poisoned map is worth
    /// less than a `500` on every subsequent tune.
    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, DiscoverResponse>> {
        self.model_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `HdHomerunHost.GetModelInfo` (v10.11.8 HdHomerunHost.cs:105-159).
    ///
    /// Reads `{api}/discover.json`, caching on `TunerHostInfo.Id`. A **404**
    /// with `throw_all_exceptions == false` is the HDHR4 case: that model has
    /// no such endpoint, so a synthetic `{ModelNumber: "HDHR"}` is returned and
    /// cached. Any other failure — and a 404 when `throw_all_exceptions` — is
    /// propagated.
    ///
    /// # Errors
    ///
    /// Fails when the URL is invalid, the device is unreachable, the response
    /// is not success (subject to the 404 rule above), or the JSON does not
    /// bind.
    pub async fn get_model_info(
        &self,
        info: &TunerHostInfo,
        throw_all_exceptions: bool,
    ) -> Result<DiscoverResponse, ServiceError> {
        let cache_key = info.id.clone().unwrap_or_default();
        if !cache_key.is_empty()
            && let Some(cached) = self.cache().get(&cache_key)
        {
            return Ok(cached.clone());
        }

        // `GetApiUrl` throws BEFORE the request, and its ArgumentException is
        // not the HttpRequestException the fallback catches — so an empty URL
        // is an error even with `throwAllExceptions: false`.
        let url = format!("{}/discover.json", get_api_url(info)?);
        let (status, body) = self.fetcher.fetch_with_status(&url).await?;

        if status == 404 && !throw_all_exceptions {
            // HDHR4 doesn't have this api.
            let fallback = DiscoverResponse {
                model_number: Some(HDHR4_FALLBACK_MODEL.to_owned()),
                ..DiscoverResponse::default()
            };
            if !cache_key.is_empty() {
                self.cache().insert(cache_key, fallback.clone());
            }
            return Ok(fallback);
        }
        if !(200..300).contains(&status) {
            // `response.EnsureSuccessStatusCode()`.
            return Err(ServiceError::backend(format!(
                "hdhomerun {url} answered {status}"
            )));
        }

        let discover: DiscoverResponse = serde_json::from_str(&body)
            .map_err(|e| LiveTvError::serialize(format!("parse {url}"), e))?;
        if !cache_key.is_empty() {
            self.cache().insert(cache_key, discover.clone());
        }
        Ok(discover)
    }

    /// `HdHomerunHost.GetLineup` (v10.11.8 HdHomerunHost.cs:74-87).
    ///
    /// `LineupURL` when the device advertised one, else `BaseURL + "/lineup.json"`,
    /// filtered to favourites when the tuner asks and always stripped of DRM
    /// channels. The response is NOT status-checked upstream — a body that is
    /// not a JSON array is a deserialization failure, which is precisely what
    /// `GetLineup_Legacy_Success` asserts.
    ///
    /// # Errors
    ///
    /// Fails when `discover.json` fails, the lineup cannot be fetched, or the
    /// body is not a JSON array of lineup entries.
    pub async fn get_lineup(
        &self,
        info: &TunerHostInfo,
    ) -> Result<Vec<LineupChannel>, ServiceError> {
        let model = self.get_model_info(info, false).await?;
        let url = model
            .lineup_url
            .clone()
            .unwrap_or_else(|| format!("{}/lineup.json", model.base_url.unwrap_or_default()));
        let (_status, body) = self.fetcher.fetch_with_status(&url).await?;
        let lineup: Vec<LineupChannel> = serde_json::from_str(&body)
            .map_err(|e| LiveTvError::serialize(format!("parse {url}"), e))?;
        Ok(lineup
            .into_iter()
            .filter(|c| !info.import_favorites_only || c.favorite)
            .filter(|c| !c.drm)
            .collect())
    }

    /// `HdHomerunHost.TryGetTunerHostInfo` (v10.11.8 HdHomerunHost.cs:527-542):
    /// the `TunerHostInfo` a discovered device turns into.
    ///
    /// # Errors
    ///
    /// Fails when the device's `discover.json` cannot be read.
    pub async fn try_get_tuner_host_info(&self, url: &str) -> Result<TunerHostInfo, ServiceError> {
        let mut host_info = TunerHostInfo {
            type_: Some(self.type_id().to_owned()),
            url: Some(url.to_owned()),
            ..TunerHostInfo::default()
        };
        let model = self.get_model_info(&host_info, false).await?;
        host_info.device_id = model.device_id;
        host_info.friendly_name = model.friendly_name;
        host_info.tuner_count = model.tuner_count;
        Ok(host_info)
    }

    /// `HdHomerunHost.GetMediaSource` (v10.11.8 HdHomerunHost.cs:182-311) — the
    /// media source for one `(channel, profile)` pair.
    ///
    /// The profile table, the HD/SD bit-rate defaults, the `mpeg2 → mpeg2video`
    /// normalization, the `NalLengthSize = "0"` marker on h264 and the
    /// `{profile}_{md5(channelId)}_{md5(apiUrl)}` id are all upstream's, byte
    /// for byte.
    ///
    /// # Errors
    ///
    /// Fails when the tuner's URL is not usable ([`get_api_url`]).
    pub fn get_media_source(
        &self,
        info: &TunerHostInfo,
        channel_id: &str,
        channel: &StoredChannel,
        profile: &str,
    ) -> Result<MediaSourceInfo, ServiceError> {
        let is_hd = channel.is_hd.unwrap_or(true);

        let matched = transcode_profile(profile);
        let is_interlaced = matched.is_none();
        let (mut width, mut height) = (None, None);
        let mut video_codec: Option<String> = None;
        let mut video_bitrate = None;

        if let Some(row) = matched {
            width = Some(row.width);
            height = Some(row.height);
            video_codec = Some("h264".to_owned());
            video_bitrate = Some(row.video_bitrate);
        } else if is_hd {
            // "This is for android tv's 1200 condition" — upstream's comment.
            width = Some(1920);
            height = Some(1080);
        }

        let mut video_codec = video_codec
            .filter(|c| !c.trim().is_empty())
            .or_else(|| channel.video_codec.clone());
        let audio_codec = channel.audio_codec.clone();

        let video_bitrate = video_bitrate.unwrap_or(if is_hd { 15_000_000 } else { 2_000_000 });
        let audio_bitrate = if is_hd { 448_000 } else { 192_000 };

        // normalize
        if video_codec
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("mpeg2"))
        {
            video_codec = Some("mpeg2video".to_owned());
        }
        let nal = video_codec
            .as_deref()
            .filter(|c| c.eq_ignore_ascii_case("h264"))
            .map(|_| "0".to_owned());

        let url = get_api_url(info)?;
        let id_profile = if profile.trim().is_empty() {
            "native"
        } else {
            profile
        };
        let id = format!(
            "{id_profile}_{}_{}",
            ferrofin_common::extensions::get_md5(channel_id).simple(),
            ferrofin_common::extensions::get_md5(&url).simple()
        );

        let mut source = MediaSourceInfo {
            path: Some(url),
            protocol: MediaProtocol::Udp,
            media_streams: vec![
                MediaStream {
                    stream_type: MediaStreamType::Video,
                    // The exact index within the container is unknown.
                    index: -1,
                    is_interlaced,
                    codec: video_codec,
                    width,
                    height,
                    bit_rate: Some(video_bitrate),
                    nal_length_size: nal,
                    ..MediaStream::default()
                },
                MediaStream {
                    stream_type: MediaStreamType::Audio,
                    index: -1,
                    codec: audio_codec,
                    bit_rate: Some(audio_bitrate),
                    ..MediaStream::default()
                },
            ],
            requires_opening: true,
            requires_closing: true,
            buffer_ms: Some(0),
            container: Some("ts".to_owned()),
            id: Some(id),
            supports_direct_play: false,
            supports_direct_stream: true,
            supports_transcoding: true,
            is_infinite_stream: true,
            ignore_dts: true,
            // All HDHR tuners require this.
            use_most_compatible_transcoding_profile: true,
            fallback_max_streaming_bitrate: Some(info.fallback_max_streaming_bitrate),
            ..MediaSourceInfo::default()
        };
        source.infer_total_bitrate(false);
        Ok(source)
    }

    /// `HdHomerunHost.DiscoverDevices`' socket half (v10.11.8 HdHomerunHost.cs:475-518).
    ///
    /// Broadcast the 20-byte discovery datagram to `255.255.255.255:65001`,
    /// then read replies until the budget runs out, accepting a datagram of
    /// more than 13 bytes whose second byte is `3` (the discover REPLY type).
    /// Each accepted sender's address becomes a candidate tuner host.
    async fn discover_addresses(&self, duration_ms: u64) -> Result<Vec<IpAddr>, ServiceError> {
        let socket = tokio::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .await
            .map_err(|e| LiveTvError::io("bind hdhomerun discovery socket", e))?;
        socket
            .set_broadcast(true)
            .map_err(|e| LiveTvError::io("enable broadcast on the discovery socket", e))?;
        socket
            .send_to(
                &DISCOVERY_DATAGRAM,
                SocketAddr::from((
                    Ipv4Addr::BROADCAST,
                    crate::hdhomerun::manager::HD_HOMERUN_PORT,
                )),
            )
            .await
            .map_err(|e| LiveTvError::io("send the hdhomerun discovery datagram", e))?;

        let mut found = Vec::new();
        let mut buffer = vec![0_u8; DISCOVERY_BUFFER];
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(duration_ms);
        // Upstream's loop runs until the linked CancellationTokenSource fires
        // and swallows the cancellation; the timeout IS the exit condition.
        while let Ok(Ok((received, from))) =
            tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
        {
            if received > 13 && buffer[1] == 3 && !found.contains(&from.ip()) {
                found.push(from.ip());
            }
        }
        Ok(found)
    }
}

#[async_trait]
impl TunerHost for HdHomerunHost {
    fn name(&self) -> &'static str {
        // `HdHomerunHost.Name` (v10.11.8 HdHomerunHost.cs:65).
        "HD Homerun"
    }

    fn type_id(&self) -> &'static str {
        // `HdHomerunHost.Type` (v10.11.8 HdHomerunHost.cs:67).
        "hdhomerun"
    }

    fn channel_id_prefix(&self) -> String {
        // Overridden upstream (HdHomerunHost.cs:69): "hdhr_", not "hdhomerun_".
        HDHR_CHANNEL_ID_PREFIX.to_owned()
    }

    /// `HdHomerunHost.GetChannelsInternal` (v10.11.8 HdHomerunHost.cs:89-103).
    async fn get_channels(&self, tuner: &TunerHostInfo) -> Result<Vec<TunerChannel>, ServiceError> {
        Ok(self
            .get_lineup(tuner)
            .await?
            .into_iter()
            .map(|c| {
                let guide_number = c.guide_number.unwrap_or_default();
                TunerChannel {
                    external_id: format!("{HDHR_CHANNEL_ID_PREFIX}{guide_number}"),
                    // `ChannelInfo.TunerChannelId` is left unset by this host,
                    // so `ListingsManager`'s match falls through to `Number` —
                    // which is the guide number. That is the key Ferrofin
                    // stores, so the same EPG entry is found.
                    guide_key: guide_number.clone(),
                    name: c.guide_name.unwrap_or_default(),
                    number: Some(guide_number),
                    image_url: None,
                    // `ChannelType.TV` unconditionally.
                    is_radio: false,
                    url: c.url.unwrap_or_default(),
                    is_hd: Some(c.hd),
                    video_codec: c.video_codec,
                    audio_codec: c.audio_codec,
                }
            })
            .collect())
    }

    /// `HdHomerunHost.Validate` (v10.11.8 HdHomerunHost.cs:441-465): clear the
    /// cache, pull the model info with `throwAllExceptions: true`, and adopt
    /// the device id. A 404 is the HDHR4 case and is swallowed.
    async fn validate(&self, info: &mut TunerHostInfo) -> Result<(), ServiceError> {
        self.cache().clear();
        match self.get_model_info(info, true).await {
            Ok(model) => {
                info.device_id = model.device_id;
                Ok(())
            }
            // HDHR4 doesn't have this api. `get_model_info` reports a non-success
            // status as a backend error naming the code, which is the only place
            // the 404 survives to.
            Err(ServiceError::Backend(msg)) if msg.ends_with("answered 404") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `HdHomerunHost.DiscoverDevices` (v10.11.8 HdHomerunHost.cs:467-525).
    async fn discover_devices(&self, duration_ms: u64) -> Result<Vec<TunerHostInfo>, ServiceError> {
        self.cache().clear();
        let mut list = Vec::new();
        for ip in self.discover_addresses(duration_ms).await? {
            match self.try_get_tuner_host_info(&format!("http://{ip}")).await {
                Ok(info) => list.push(info),
                // `TryGetTunerHostInfo` is awaited inside the receive loop and a
                // device that answers the broadcast but not HTTP would abort
                // the whole scan; upstream's outer `catch (Exception ex)` logs
                // and stops. Skipping the one device keeps the scan honest
                // without inventing a host that did not answer.
                Err(error) => {
                    tracing::warn!(%ip, %error, "live tv: hdhomerun device did not answer discover.json");
                }
            }
        }
        Ok(list)
    }

    /// `HdHomerunHost.GetChannelStreamMediaSources` (v10.11.8 HdHomerunHost.cs:339-379).
    async fn channel_media_sources(
        &self,
        tuner: &TunerHostInfo,
        channel: &StoredChannel,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        let hdhr_id = hdhr_id_from_channel_id(&channel.external_id).to_owned();
        let mut list = Vec::new();

        if is_legacy_tuner(&channel.path) {
            list.push(self.get_media_source(tuner, &hdhr_id, channel, "native")?);
            return Ok(list);
        }

        let model = self.get_model_info(tuner, false).await?;
        if model.supports_transcoding() {
            if tuner.allow_hw_transcoding {
                for profile in HW_TRANSCODING_PROFILES {
                    list.push(self.get_media_source(tuner, &hdhr_id, channel, profile)?);
                }
            }
            list.push(self.get_media_source(tuner, &hdhr_id, channel, "native")?);
        }

        if list.is_empty() {
            list.push(self.get_media_source(tuner, &hdhr_id, channel, "native")?);
        }
        Ok(list)
    }

    /// `HdHomerunHost.GetChannelStream` (v10.11.8 HdHomerunHost.cs:381-439).
    ///
    /// The stream id's profile is its part before the first `_` (upstream's
    /// `streamId.AsSpan().LeftPart('_')`), forced to `native` on a device that
    /// cannot transcode. A legacy lineup entry becomes the UDP plan; everything
    /// else is a shared HTTP stream on the lineup URL, with `?transcode=` when
    /// a real profile was picked.
    async fn channel_stream(
        &self,
        tuner: &TunerHostInfo,
        channel: &StoredChannel,
        stream_id: Option<&str>,
    ) -> Result<ChannelStream, ServiceError> {
        let mut profile = stream_id
            .unwrap_or_default()
            .split('_')
            .next()
            .unwrap_or_default()
            .to_owned();
        let hdhr_id = hdhr_id_from_channel_id(&channel.external_id).to_owned();
        let model = self.get_model_info(tuner, false).await?;
        if !model.supports_transcoding() {
            "native".clone_into(&mut profile);
        }

        let mut source = self.get_media_source(tuner, &hdhr_id, channel, &profile)?;

        if is_legacy_tuner(&channel.path) {
            // `new Uri(mediaSource.Path).Host` in `HdHomerunUdpStream.Open`;
            // `mediaSource.Path` is still the device's api URL at this point.
            let remote = device_ip(source.path.as_deref().unwrap_or_default())?;
            return Ok(ChannelStream {
                source,
                kind: ChannelStreamKind::LegacyUdp(Box::new(LegacyUdpPlan {
                    device: SocketAddr::new(remote, crate::hdhomerun::manager::HD_HOMERUN_PORT),
                    num_tuners: model.tuner_count,
                    commands: crate::hdhomerun::manager::legacy_channel_commands(&channel.path),
                })),
            });
        }

        source.protocol = MediaProtocol::Http;
        let mut http_url = channel.path.clone();
        // If raw was used, the tuner doesn't support params.
        if !profile.trim().is_empty() && !profile.eq_ignore_ascii_case("native") {
            http_url.push_str("?transcode=");
            http_url.push_str(&profile);
        }
        source.path = Some(http_url);
        Ok(ChannelStream {
            source,
            kind: ChannelStreamKind::Http,
        })
    }
}

/// `HdHomerunChannelInfo.IsLegacyTuner` (v10.11.8 HdHomerunHost.cs:100):
/// `(i.URL ?? string.Empty).StartsWith("hdhomerun", StringComparison.OrdinalIgnoreCase)`.
#[must_use]
pub fn is_legacy_tuner(url: &str) -> bool {
    url.len() >= 9 && url[..9].eq_ignore_ascii_case("hdhomerun")
}

/// The device address out of an api URL, for the legacy UDP control path.
///
/// `HdHomerunUdpStream.Open` does `IPAddress.Parse(new Uri(mediaSource.Path).Host)`
/// (v10.11.8 HdHomerunUdpStream.cs:90), so a hostname that is not a literal IP
/// is an error there too.
fn device_ip(api_url: &str) -> Result<IpAddr, ServiceError> {
    let host = api_url
        .split_once("://")
        .map_or(api_url, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Strip a port, and the brackets an IPv6 authority carries.
    let host = host.rsplit_once(':').map_or(host, |(h, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
            h
        } else {
            host
        }
    });
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<IpAddr>().map_err(|_| {
        ServiceError::InvalidInput(format!("hdhomerun device address {host} is not an IP"))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        HdHomerunHost, LegacyUdpPlan, device_ip, get_api_url, hdhr_id_from_channel_id,
        is_legacy_tuner,
    };
    use crate::fetch::SourceFetcher;
    use crate::tuner_host::{ChannelStreamKind, StoredChannel, TunerHost};
    use ferrofin_model::live_tv::TunerHostInfo;
    use ferrofin_traits::error::ServiceError;
    use rstest::rstest;

    /// A device that answers `discover.json`/`lineup.json` from two canned
    /// bodies, keyed by the last path segment.
    struct Canned {
        /// The `discover.json` body.
        discover: String,
        /// The `lineup.json` body.
        lineup: String,
    }

    #[async_trait::async_trait]
    impl SourceFetcher for Canned {
        async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
            if url.ends_with("discover.json") {
                Ok(self.discover.clone())
            } else {
                Ok(self.lineup.clone())
            }
        }
    }

    fn host_with(model: &str) -> HdHomerunHost {
        HdHomerunHost::new(Arc::new(Canned {
            discover: format!(
                r#"{{"ModelNumber":"{model}","TunerCount":2,"BaseURL":"http://10.0.0.5:80"}}"#
            ),
            lineup: r#"[{"GuideNumber":"4.1","GuideName":"WCMH","HD":1,
                         "VideoCodec":"MPEG2","AudioCodec":"AC3",
                         "URL":"http://10.0.0.5:5004/auto/v4.1"}]"#
                .to_owned(),
        }))
    }

    fn tuner(url: &str) -> TunerHostInfo {
        TunerHostInfo {
            url: Some(url.to_owned()),
            ..TunerHostInfo::default()
        }
    }

    #[rstest]
    // `GetApiUrl`: prepend the scheme, normalize, trim the trailing slash.
    #[case("192.168.1.182", "http://192.168.1.182")]
    #[case("192.168.1.182:80", "http://192.168.1.182:80")]
    #[case("http://192.168.1.182/", "http://192.168.1.182")]
    #[case("HTTP://192.168.1.182", "http://192.168.1.182")]
    #[case("https://device.local", "https://device.local")]
    #[case("  192.168.1.182  ", "http://192.168.1.182")]
    fn the_api_url_is_normalized_like_absolute_uri(#[case] raw: &str, #[case] expected: &str) {
        assert_eq!(get_api_url(&tuner(raw)).expect("url"), expected);
    }

    #[rstest]
    #[case("")]
    #[case("   ")]
    fn a_blank_api_url_is_invalid_tuner_info(#[case] raw: &str) {
        // `throw new ArgumentException("Invalid tuner info")`.
        assert!(matches!(
            get_api_url(&tuner(raw)),
            Err(ServiceError::InvalidInput(ref m)) if m == "Invalid tuner info"
        ));
    }

    #[rstest]
    // `(i.URL ?? "").StartsWith("hdhomerun", OrdinalIgnoreCase)`.
    #[case("hdhomerun://1040A0A1-0/ch4-1", true)]
    #[case("HDHomeRun://1040A0A1-0/ch4", true)]
    #[case("http://10.0.0.5:5004/auto/v4.1", false)]
    #[case("", false)]
    #[case("hdhomeru", false)]
    fn a_legacy_lineup_entry_is_recognised_by_its_scheme(
        #[case] url: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(is_legacy_tuner(url), expected);
    }

    #[rstest]
    // `channelId.Split('_')[1]`.
    #[case("hdhr_4.1", "4.1")]
    #[case("hdhr_10.2", "10.2")]
    #[case("nounderscore", "")]
    fn the_hdhr_id_is_the_part_after_the_prefix(#[case] id: &str, #[case] expected: &str) {
        assert_eq!(hdhr_id_from_channel_id(id), expected);
    }

    #[rstest]
    #[case("http://10.0.0.5:80", "10.0.0.5")]
    #[case("http://10.0.0.5", "10.0.0.5")]
    #[case("http://[::1]:80", "::1")]
    fn the_device_address_is_parsed_out_of_the_api_url(#[case] url: &str, #[case] ip: &str) {
        assert_eq!(device_ip(url).expect("ip").to_string(), ip);
    }

    #[test]
    fn a_hostname_is_not_a_device_address() {
        // `IPAddress.Parse(new Uri(...).Host)` throws on a name upstream too.
        assert!(device_ip("http://device.local:80").is_err());
    }

    fn channel() -> StoredChannel {
        StoredChannel {
            external_id: "hdhr_4.1".to_owned(),
            path: "http://10.0.0.5:5004/auto/v4.1".to_owned(),
            is_hd: Some(true),
            video_codec: Some("MPEG2".to_owned()),
            audio_codec: Some("AC3".to_owned()),
        }
    }

    #[rstest]
    // The profile table, verbatim from `GetMediaSource`. Every transcoding
    // profile is progressive h264 (so `NalLengthSize = "0"`); the native one
    // keeps the tuner's own codec, is interlaced, and gets the android-tv
    // 1200-condition resolution when the channel is HD.
    #[case("mobile", 1280, 720, 2_000_000, "h264")]
    #[case("heavy", 1920, 1080, 15_000_000, "h264")]
    #[case("internet720", 1280, 720, 8_000_000, "h264")]
    #[case("internet540", 960, 540, 2_500_000, "h264")]
    #[case("internet480", 848, 480, 2_000_000, "h264")]
    #[case("internet360", 640, 360, 1_500_000, "h264")]
    #[case("internet240", 432, 240, 1_000_000, "h264")]
    #[case("native", 1920, 1080, 15_000_000, "mpeg2video")]
    #[case("HEAVY", 1920, 1080, 15_000_000, "h264")]
    fn the_transcode_profile_table_is_upstreams(
        #[case] profile: &str,
        #[case] width: i32,
        #[case] height: i32,
        #[case] bitrate: i32,
        #[case] codec: &str,
    ) {
        let host = host_with("HDTC-2US");
        let source = host
            .get_media_source(&tuner("10.0.0.5"), "4.1", &channel(), profile)
            .expect("source");
        let video = &source.media_streams[0];
        assert_eq!((video.width, video.height), (Some(width), Some(height)));
        assert_eq!(video.bit_rate, Some(bitrate));
        assert_eq!(video.codec.as_deref(), Some(codec));
        // `mpeg2 → mpeg2video`, and the NAL marker only on h264.
        assert_eq!(
            video.nal_length_size.as_deref(),
            if codec == "h264" { Some("0") } else { None }
        );
        // A transcoding profile is progressive; native is interlaced-unknown.
        assert_eq!(video.is_interlaced, codec == "mpeg2video");
        // `isHd` ⇒ 448 kbps audio.
        assert_eq!(source.media_streams[1].bit_rate, Some(448_000));
        // `{profile}_{md5(channelId)}_{md5(apiUrl)}`.
        let expected_id = format!(
            "{profile}_{}_{}",
            ferrofin_common::extensions::get_md5("4.1").simple(),
            ferrofin_common::extensions::get_md5("http://10.0.0.5").simple()
        );
        assert_eq!(source.id.as_deref(), Some(expected_id.as_str()));
    }

    #[test]
    fn an_sd_channel_takes_the_sd_defaults_and_no_dummy_resolution() {
        let host = host_with("HDHR3-US");
        let sd = StoredChannel {
            is_hd: Some(false),
            ..channel()
        };
        let source = host
            .get_media_source(&tuner("10.0.0.5"), "4.1", &sd, "native")
            .expect("source");
        assert_eq!(source.media_streams[0].width, None);
        assert_eq!(source.media_streams[0].height, None);
        assert_eq!(source.media_streams[0].bit_rate, Some(2_000_000));
        assert_eq!(source.media_streams[1].bit_rate, Some(192_000));
    }

    #[tokio::test]
    async fn a_transcoding_device_offers_every_profile_in_upstreams_order() {
        // `GetChannelStreamMediaSources`: heavy, internet540/480/360/240,
        // mobile, then native — but only when the model transcodes AND the
        // tuner allows hardware transcoding.
        let host = host_with("HDTC-2US");
        let mut info = tuner("10.0.0.5");
        info.allow_hw_transcoding = true;
        let sources = host
            .channel_media_sources(&info, &channel())
            .await
            .expect("sources");
        let profiles: Vec<_> = sources
            .iter()
            .map(|s| {
                s.id.as_deref()
                    .unwrap_or_default()
                    .split('_')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            profiles,
            [
                "heavy",
                "internet540",
                "internet480",
                "internet360",
                "internet240",
                "mobile",
                "native"
            ]
        );

        info.allow_hw_transcoding = false;
        let native_only = host
            .channel_media_sources(&info, &channel())
            .await
            .expect("sources");
        assert_eq!(native_only.len(), 1);
    }

    #[tokio::test]
    async fn a_non_transcoding_device_offers_the_native_profile_alone() {
        let host = host_with("HDHR3-US");
        let mut info = tuner("10.0.0.5");
        info.allow_hw_transcoding = true;
        let sources = host
            .channel_media_sources(&info, &channel())
            .await
            .expect("sources");
        assert_eq!(sources.len(), 1);
        assert!(
            sources[0]
                .id
                .as_deref()
                .is_some_and(|id| id.starts_with("native_"))
        );
    }

    #[tokio::test]
    async fn a_legacy_lineup_entry_offers_the_native_profile_alone() {
        // The legacy branch short-circuits BEFORE `discover.json` is consulted.
        let host = host_with("HDTC-2US");
        let mut info = tuner("10.0.0.5");
        info.allow_hw_transcoding = true;
        let legacy = StoredChannel {
            path: "hdhomerun://1040A0A1-0/ch4-1".to_owned(),
            ..channel()
        };
        let sources = host
            .channel_media_sources(&info, &legacy)
            .await
            .expect("sources");
        assert_eq!(sources.len(), 1);
    }

    #[tokio::test]
    async fn the_http_stream_appends_the_transcode_query_only_for_a_real_profile() {
        let host = host_with("HDTC-2US");
        let info = tuner("10.0.0.5");
        for (stream_id, expected) in [
            (
                Some("heavy_abc_def"),
                "http://10.0.0.5:5004/auto/v4.1?transcode=heavy",
            ),
            (Some("native_abc_def"), "http://10.0.0.5:5004/auto/v4.1"),
            (None, "http://10.0.0.5:5004/auto/v4.1"),
        ] {
            let chosen = host
                .channel_stream(&info, &channel(), stream_id)
                .await
                .expect("stream");
            assert_eq!(chosen.kind, ChannelStreamKind::Http);
            assert_eq!(chosen.source.path.as_deref(), Some(expected));
        }
    }

    #[tokio::test]
    async fn a_device_that_cannot_transcode_forces_the_native_profile() {
        // "If raw was used, the tuner doesn't support params" — a client asking
        // for `heavy` on an HDHR3 must not get `?transcode=heavy` appended.
        let host = host_with("HDHR3-US");
        let chosen = host
            .channel_stream(&tuner("10.0.0.5"), &channel(), Some("heavy_abc_def"))
            .await
            .expect("stream");
        assert_eq!(
            chosen.source.path.as_deref(),
            Some("http://10.0.0.5:5004/auto/v4.1")
        );
    }

    #[tokio::test]
    async fn a_legacy_channel_becomes_the_udp_plan_with_its_parsed_commands() {
        let host = host_with("HDHR3-US");
        let legacy = StoredChannel {
            path: "hdhomerun://1040A0A1-0/ch4-1".to_owned(),
            ..channel()
        };
        let chosen = host
            .channel_stream(&tuner("10.0.0.5"), &legacy, None)
            .await
            .expect("stream");
        let ChannelStreamKind::LegacyUdp(plan) = chosen.kind else {
            panic!("a hdhomerun:// entry must take the UDP control path");
        };
        assert_eq!(
            *plan,
            LegacyUdpPlan {
                device: "10.0.0.5:65001".parse().expect("addr"),
                num_tuners: 2,
                commands: vec![
                    ("channel".to_owned(), "4".to_owned()),
                    ("program".to_owned(), "1".to_owned()),
                ],
            }
        );
    }

    #[tokio::test]
    async fn discovery_finds_nothing_when_no_device_answers() {
        // An empty list is the honest answer on a network with no HDHomeRun on
        // it, and it must come back within the budget rather than hanging. A
        // sandbox that refuses the broadcast is an Err, which the manager logs
        // and turns into the same empty list — either way, never an invented
        // device.
        let host = host_with("HDHR3-US");
        let started = std::time::Instant::now();
        let found = host.discover_devices(150).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(found.map_or(true, |d| d.is_empty()));
    }
}
