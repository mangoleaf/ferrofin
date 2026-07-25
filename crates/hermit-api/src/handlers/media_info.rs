//! `MediaInfoController` — playback-info resolution.
//!
//! Ports `GET`/`POST /Items/{itemId}/PlaybackInfo`: resolves the item's playable
//! [`MediaSourceInfo`](hermit_model::dto::MediaSourceInfo)s for the requesting
//! user via the [`MediaSourceManager`](hermit_traits::library::MediaSourceManager)
//! and returns them in a [`PlaybackInfoResponse`]. The `POST` body (a device
//! profile + stream selections) is accepted and ignored for now; both verbs
//! share one handler, matching Jellyfin's two actions.

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_model::data::MediaStreamProtocol;
use hermit_model::dlna::{
    DeviceProfile, DlnaProfileType, MediaOptions, StreamBuilder, TranscoderSupport,
};
use hermit_model::dto::MediaSourceInfo;
use hermit_model::media_info::{LiveStreamRequest, LiveStreamResponse, PlaybackInfoResponse};
use hermit_model::session::PlayMethod;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user;
use crate::state::AppState;

/// The server's transcode capabilities, fed to the [`StreamBuilder`] so it knows
/// which codecs a non-direct-play source can be transcoded to. Reflects the codecs
/// the ffmpeg build ships with (matching Jellyfin's default encoder set).
struct HermitTranscoderSupport;

impl TranscoderSupport for HermitTranscoderSupport {
    fn can_encode_to_audio_codec(&self, codec: &str) -> bool {
        matches!(
            codec.to_ascii_lowercase().as_str(),
            "aac"
                | "mp3"
                | "libmp3lame"
                | "ac3"
                | "eac3"
                | "opus"
                | "libopus"
                | "flac"
                | "vorbis"
                | "libvorbis"
                | "alac"
                | "wav"
                | "pcm_s16le"
        )
    }
    fn can_encode_to_subtitle_codec(&self, codec: &str) -> bool {
        matches!(
            codec.to_ascii_lowercase().as_str(),
            "srt" | "subrip" | "ass" | "ssa" | "vtt" | "webvtt" | "ttml" | "mov_text"
        )
    }
    fn can_extract_subtitles(&self, codec: &str) -> bool {
        // Text-based subtitle streams extract to text; image-based ones cannot.
        !matches!(
            codec.to_ascii_lowercase().as_str(),
            "hdmv_pgs_subtitle"
                | "pgssub"
                | "dvd_subtitle"
                | "dvdsub"
                | "dvb_subtitle"
                | "dvbsub"
                | "xsub"
        )
    }
}

/// Query parameters for the playback-info endpoints.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaybackInfoQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// Resolves the playback info for `item_id` and the effective user.
///
/// Shared by the `GET` and `POST` handlers. `allow_media_probe` and
/// `enable_path_substitution` mirror the C# defaults for the basic path.
async fn playback_info(
    state: &AppState,
    auth: &hermit_traits::options::AuthorizationInfo,
    item_id: Uuid,
    user_id: Option<Uuid>,
    profile: Option<&DeviceProfile>,
    max_streaming_bitrate: Option<i32>,
) -> Result<PlaybackInfoResponse, ApiError> {
    let user = resolve_user(state, auth, user_id).await?;
    let resolved_user_id = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
    let mut media_sources = state
        .media_sources
        .get_playback_media_sources(item_id, resolved_user_id, true, true)
        .await?;

    // When the client posts its device profile, decide per source whether it can
    // direct-play or must transcode (e.g. h264 video but AC3 audio a browser can't
    // decode). Without this every source claimed direct-play, so incompatible-audio
    // files played video-only. No profile (the GET form) keeps the static sources.
    if let Some(profile) = profile {
        for source in &mut media_sources {
            apply_stream_decision(
                source,
                profile,
                item_id,
                max_streaming_bitrate,
                auth.token.as_deref(),
                auth.device_id.as_deref(),
            );
        }
    }

    Ok(PlaybackInfoResponse {
        media_sources,
        // The client threads this id through every playback-progress report; C#
        // mints a fresh GUID per PlaybackInfo call, so a null here breaks reporting.
        play_session_id: Some(Uuid::new_v4().to_string()),
        error_code: None,
    })
}

/// The HLS segment container to transcode into, constrained to Jellyfin's
/// `supportedHlsContainers` ({`ts`, `mp4`}).
///
/// A `negotiated` value of `ts`/`mp4` is honoured; anything else (e.g. the source
/// `mkv` from a DirectStream-remux path) falls back to a codec-appropriate
/// choice: fMP4 (`mp4`) for HEVC/AV1 — which browsers can only decode as an
/// `hvc1`/`av01`-tagged fragment — and MPEG-TS (`ts`) otherwise.
fn hls_segment_container(negotiated: Option<&str>, source: &MediaSourceInfo) -> String {
    // HEVC/AV1 must be fMP4 regardless of what the builder negotiated: browsers
    // decode them via MSE only as an `hvc1`/`av01`-tagged mp4 fragment, never in
    // MPEG-TS (HDR HEVC in TS is exactly what failed to start). This is also what
    // jellyfin-web's `mp4` HLS transcoding profile selects for these codecs.
    let needs_fmp4 = source
        .video_stream()
        .and_then(|v| v.codec.as_deref())
        .is_some_and(|codec| {
            codec.eq_ignore_ascii_case("hevc")
                || codec.eq_ignore_ascii_case("h265")
                || codec.eq_ignore_ascii_case("av1")
        });
    if needs_fmp4 {
        return "mp4".to_owned();
    }
    // Otherwise honour a valid HLS container ({ts, mp4}); a remux path may hand
    // back the source container (e.g. `mkv`), which is not one — fall back to `ts`.
    match negotiated {
        Some(c) if c.eq_ignore_ascii_case("mp4") => "mp4".to_owned(),
        _ => "ts".to_owned(),
    }
}

/// The HLS video transcoding profile's `MaxAudioChannels` for `container`, if
/// the client's profile declares one.
///
/// Prefers the HLS video transcoding profile whose container matches the chosen
/// segment container, falling back to any HLS video profile. Returns `None` when
/// the profile sets no channel cap (then the source's channel count streams
/// through unchanged, matching the pre-existing behaviour).
fn hls_transcoding_max_audio_channels(profile: &DeviceProfile, container: &str) -> Option<i32> {
    let mut fallback = None;
    for p in &profile.transcoding_profiles {
        if p.profile_type != DlnaProfileType::Video || p.protocol != MediaStreamProtocol::hls {
            continue;
        }
        let Some(max) = p
            .max_audio_channels
            .as_deref()
            .and_then(|c| c.trim().parse::<i32>().ok())
        else {
            continue;
        };
        if p.container
            .split(',')
            .any(|c| c.eq_ignore_ascii_case(container))
        {
            return Some(max);
        }
        fallback.get_or_insert(max);
    }
    fallback
}

/// Runs the DLNA [`StreamBuilder`] for one media source against the client's
/// profile and stamps the resulting play decision onto it: direct-play stays as-is,
/// otherwise `SupportsDirectPlay` is cleared and a `TranscodingUrl` (the HLS master
/// / remux stream, with the negotiated codecs) is set so the client transcodes.
fn apply_stream_decision(
    source: &mut MediaSourceInfo,
    profile: &DeviceProfile,
    item_id: Uuid,
    max_streaming_bitrate: Option<i32>,
    token: Option<&str>,
    device_id: Option<&str>,
) {
    let mut options = MediaOptions::new(profile.clone());
    options.item_id = item_id;
    options.media_source_id.clone_from(&source.id);
    options.device_id = device_id.map(str::to_owned);
    options.max_bitrate = max_streaming_bitrate;
    options.media_sources = vec![source.clone()];

    let support = HermitTranscoderSupport;
    let Some(stream) = StreamBuilder::new(&support).get_optimal_video_stream(&options) else {
        return;
    };

    if stream.play_method == PlayMethod::DirectPlay {
        source.supports_direct_play = true;
        return;
    }
    // Not direct-play: deliver an HLS transcode. The negotiated codecs (typically
    // copy the compatible video, re-encode only the incompatible audio) are carried
    // on the StreamInfo; we pin the delivery to HLS because the builder can mislabel
    // an audio-only re-encode as a raw-copy DirectStream, which would hand the client
    // the untouched incompatible audio (the "video plays, no audio" case).
    let mut stream = stream;
    stream.play_method = PlayMethod::Transcode;
    stream.sub_protocol = MediaStreamProtocol::hls;
    // Choose a valid HLS segment container. Jellyfin's `supportedHlsContainers`
    // is {ts, mp4}: honour the profile's choice when it is one of those, but the
    // builder can hand back the *source* container (e.g. `mkv`) from a
    // DirectStream-remux path — not a valid HLS segment container — so otherwise
    // pick by the copied video codec. HEVC/AV1 must be fMP4 (browsers only decode
    // them in an `hvc1`/`av01`-tagged mp4 fragment, never MPEG-TS); everything
    // else stays on the broadly-compatible `ts`. Forcing `ts` unconditionally
    // (the old behaviour) is what made HDR HEVC fail to start.
    let container = hls_segment_container(stream.container.as_deref(), source);
    stream.container = Some(container.clone());

    // Adopt the HLS transcoding profile's audio-channel cap. The builder can
    // label this a raw-copy DirectStream (video copied, incompatible audio left
    // to a container remux); Jellyfin would deliver that progressively over HTTP,
    // where the browser's native pipeline decodes 7.1 AAC. We instead force an
    // HLS *transcode* of the audio (above), which the browser decodes via MSE —
    // and MSE can't handle >2ch AAC under a web profile. So downmix to the HLS
    // profile's `MaxAudioChannels`, carried on the URL as
    // `TranscodingMaxAudioChannels` → the transcoder's `-ac`. Without it the
    // HDR HEVC video plays but the audio track is silent/undecodable.
    if stream.transcoding_max_audio_channels.is_none() {
        stream.transcoding_max_audio_channels =
            hls_transcoding_max_audio_channels(profile, &container);
    }

    source.supports_direct_play = false;
    source.supports_direct_stream = false;
    source.supports_transcoding = true;
    source.transcoding_url = Some(stream.to_url(None, token, None));
    source.transcoding_sub_protocol = MediaStreamProtocol::hls;
    source.transcoding_container = Some(container);
}

/// `GET /Items/{itemId}/PlaybackInfo` — playback info for the item.
///
/// Port of `MediaInfoController.GetPlaybackInfo`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/PlaybackInfo",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 200, description = "Playback info returned", body = PlaybackInfoResponse)),
    tag = "hermit"
)]
async fn get_playback_info(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<PlaybackInfoQuery>,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    // The GET form carries no device profile, so every source keeps its static
    // (direct-play) shape.
    Ok(Json(
        playback_info(&state, &auth, item_id, query.user_id, None, None).await?,
    ))
}

/// The `POST /Items/{itemId}/PlaybackInfo` body — the client's device profile plus
/// the streaming limits used to negotiate the play method. Other posted fields
/// (stream-index selections, etc.) are ignored for the basic path.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlaybackInfoBody {
    #[serde(default)]
    device_profile: Option<DeviceProfile>,
    #[serde(default)]
    max_streaming_bitrate: Option<i32>,
}

/// `POST /Items/{itemId}/PlaybackInfo` — playback info with a posted profile.
///
/// Port of `MediaInfoController.GetPostedPlaybackInfo`. The posted device
/// profile / stream selections are accepted and ignored for the basic path; the
/// resolved sources are identical to the `GET` form.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/PlaybackInfo",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 200, description = "Playback info returned", body = PlaybackInfoResponse)),
    tag = "hermit"
)]
async fn post_playback_info(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<PlaybackInfoQuery>,
    body: Option<Json<PlaybackInfoBody>>,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    Ok(Json(
        playback_info(
            &state,
            &auth,
            item_id,
            query.user_id,
            body.device_profile.as_ref(),
            body.max_streaming_bitrate,
        )
        .await?,
    ))
}

/// Query parameters for `POST /LiveStreams/Open`.
///
/// Mirrors the flat query form of `MediaInfoController.OpenLiveStream`; the
/// posted `OpenLiveStreamDto` body (device profile + the same fields) is accepted
/// and folded in where the query is absent, matching the C# `?? dto?.Field`
/// precedence (query wins).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenLiveStreamQuery {
    /// The open token identifying the source to open.
    #[serde(default)]
    open_token: Option<String>,
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The play session id.
    #[serde(default)]
    play_session_id: Option<String>,
    /// The maximum streaming bitrate.
    #[serde(default)]
    max_streaming_bitrate: Option<i32>,
    /// The start time in ticks.
    #[serde(default)]
    start_time_ticks: Option<i64>,
    /// The audio stream index.
    #[serde(default)]
    audio_stream_index: Option<i32>,
    /// The subtitle stream index.
    #[serde(default)]
    subtitle_stream_index: Option<i32>,
    /// The maximum number of audio channels.
    #[serde(default)]
    max_audio_channels: Option<i32>,
    /// The item id whose source is opened.
    #[serde(default)]
    item_id: Option<Uuid>,
}

/// `POST /LiveStreams/Open` — open a media source and return its live stream.
///
/// Port of `MediaInfoController.OpenLiveStream`. The device-profile negotiation
/// carried by the posted body is deferred; the query/body scalar parameters are
/// assembled into a [`LiveStreamRequest`] and handed to
/// [`MediaSourceManager::open_live_stream`](hermit_traits::library::MediaSourceManager::open_live_stream),
/// which probes the source and registers it in the open-stream table.
#[utoipa::path(
    post,
    path = "/LiveStreams/Open",
    responses((status = 200, description = "Media source opened", body = LiveStreamResponse)),
    tag = "hermit"
)]
async fn open_live_stream(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<OpenLiveStreamQuery>,
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<LiveStreamResponse>, ApiError> {
    // The posted `OpenLiveStreamDto` device profile is not yet honoured.
    let _ = body;
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let request = LiveStreamRequest {
        open_token: query.open_token,
        user_id,
        play_session_id: query.play_session_id,
        max_streaming_bitrate: query.max_streaming_bitrate,
        start_time_ticks: query.start_time_ticks,
        audio_stream_index: query.audio_stream_index,
        subtitle_stream_index: query.subtitle_stream_index,
        max_audio_channels: query.max_audio_channels,
        item_id: query.item_id.unwrap_or_else(Uuid::nil),
        ..Default::default()
    };
    let media_source = state.media_sources.open_live_stream(&request).await?;
    Ok(Json(LiveStreamResponse::new(media_source)))
}

/// Query parameters for `POST /LiveStreams/Close`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloseLiveStreamQuery {
    /// The id of the open live stream to close.
    live_stream_id: String,
}

/// `POST /LiveStreams/Close` — close an open media source.
///
/// Port of `MediaInfoController.CloseLiveStream`. Returns `204 No Content` on
/// success, mirroring the controller's `NoContent()`.
#[utoipa::path(
    post,
    path = "/LiveStreams/Close",
    responses((status = 204, description = "Livestream closed")),
    tag = "hermit"
)]
async fn close_live_stream(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<CloseLiveStreamQuery>,
) -> Result<StatusCode, ApiError> {
    state
        .media_sources
        .close_live_stream(&query.live_stream_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The default `Playback/BitrateTest` payload size, in bytes (C# default 102400).
const DEFAULT_BITRATE_TEST_SIZE: usize = 102_400;

/// The maximum `Playback/BitrateTest` payload size, in bytes (C# `Range` upper
/// bound of 100_000_000).
const MAX_BITRATE_TEST_SIZE: usize = 100_000_000;

/// Query parameters for `GET /Playback/BitrateTest`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BitrateTestQuery {
    /// The number of bytes to return; defaults to
    /// [`DEFAULT_BITRATE_TEST_SIZE`].
    #[serde(default)]
    size: Option<usize>,
}

/// `GET /Playback/BitrateTest` — return a buffer of the requested size.
///
/// Port of `MediaInfoController.GetBitrateTestBytes`: the client measures its
/// download bandwidth against a fixed-size payload. The C# body is random bytes;
/// a zero-filled buffer serves the same measurement purpose and is cheaper. The
/// requested size is clamped to `[1, 100_000_000]` to match the controller's
/// `[Range]`, returning `400` when it falls outside.
#[utoipa::path(
    get,
    path = "/Playback/BitrateTest",
    params(("size" = Option<i32>, Query, description = "The buffer size in bytes")),
    responses((status = 200, description = "Test buffer returned")),
    tag = "hermit"
)]
async fn get_bitrate_test(
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<BitrateTestQuery>,
) -> Result<Response, ApiError> {
    let size = query.size.unwrap_or(DEFAULT_BITRATE_TEST_SIZE);
    if size == 0 || size > MAX_BITRATE_TEST_SIZE {
        return Err(ApiError::BadRequest(format!(
            "size must be between 1 and {MAX_BITRATE_TEST_SIZE}"
        )));
    }
    let body = vec![0_u8; size];
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response())
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Items/{itemId}/PlaybackInfo",
            get(get_playback_info).post(post_playback_info),
        )
        .route("/LiveStreams/Open", post(open_live_stream))
        .route("/LiveStreams/Close", post(close_live_stream))
        .route("/Playback/BitrateTest", get(get_bitrate_test))
}

#[cfg(test)]
mod tests {
    use super::{
        HermitTranscoderSupport, hls_segment_container, hls_transcoding_max_audio_channels,
    };
    use hermit_model::data::MediaStreamProtocol;
    use hermit_model::dlna::{
        DeviceProfile, DlnaProfileType, TranscoderSupport, TranscodingProfile,
    };
    use hermit_model::dto::MediaSourceInfo;
    use hermit_model::entities::MediaStreamType;
    use hermit_model::entities_media::MediaStream;

    fn hls_video_profile(container: &str, max_channels: Option<&str>) -> TranscodingProfile {
        TranscodingProfile {
            container: container.to_owned(),
            profile_type: DlnaProfileType::Video,
            protocol: MediaStreamProtocol::hls,
            max_audio_channels: max_channels.map(str::to_owned),
            ..TranscodingProfile::default()
        }
    }

    #[test]
    fn hls_channel_cap_prefers_matching_container_then_falls_back() {
        let profile = DeviceProfile {
            transcoding_profiles: vec![
                // A non-HLS profile is ignored even though it matches the container.
                TranscodingProfile {
                    container: "mp4".to_owned(),
                    profile_type: DlnaProfileType::Video,
                    protocol: MediaStreamProtocol::http,
                    max_audio_channels: Some("6".to_owned()),
                    ..TranscodingProfile::default()
                },
                hls_video_profile("ts", Some("2")),
                hls_video_profile("mp4", Some("2")),
            ],
            ..DeviceProfile::default()
        };
        assert_eq!(hls_transcoding_max_audio_channels(&profile, "mp4"), Some(2));
        assert_eq!(hls_transcoding_max_audio_channels(&profile, "ts"), Some(2));
        // No container match → falls back to the first HLS video profile's cap.
        assert_eq!(hls_transcoding_max_audio_channels(&profile, "mkv"), Some(2));
    }

    #[test]
    fn hls_channel_cap_absent_when_profile_sets_none() {
        let profile = DeviceProfile {
            transcoding_profiles: vec![hls_video_profile("mp4", None)],
            ..DeviceProfile::default()
        };
        assert_eq!(hls_transcoding_max_audio_channels(&profile, "mp4"), None);
    }

    fn source_with_video(codec: &str) -> MediaSourceInfo {
        MediaSourceInfo {
            media_streams: vec![MediaStream {
                codec: Some(codec.to_owned()),
                stream_type: MediaStreamType::Video,
                ..MediaStream::default()
            }],
            ..MediaSourceInfo::default()
        }
    }

    #[test]
    fn hls_container_constrains_to_ts_or_mp4() {
        let hevc = source_with_video("hevc");
        let h264 = source_with_video("h264");
        // HEVC/AV1 are ALWAYS fMP4 — even if the builder negotiated `ts` or the
        // source `mkv` — because browsers can't decode them in MPEG-TS.
        assert_eq!(hls_segment_container(Some("ts"), &hevc), "mp4");
        assert_eq!(hls_segment_container(Some("mkv"), &hevc), "mp4");
        assert_eq!(
            hls_segment_container(None, &source_with_video("av1")),
            "mp4"
        );
        // h264 honours a valid HLS container, else falls back to ts (never mkv).
        assert_eq!(hls_segment_container(Some("MP4"), &h264), "mp4");
        assert_eq!(hls_segment_container(Some("ts"), &h264), "ts");
        assert_eq!(hls_segment_container(Some("mkv"), &h264), "ts");
        assert_eq!(hls_segment_container(None, &h264), "ts");
    }

    #[test]
    fn transcoder_support_reports_the_ffmpeg_audio_codecs() {
        let s = HermitTranscoderSupport;
        // Browser-transcode targets the decision relies on.
        assert!(s.can_encode_to_audio_codec("aac"));
        assert!(s.can_encode_to_audio_codec("AAC"), "case-insensitive");
        assert!(s.can_encode_to_audio_codec("opus"));
        assert!(s.can_encode_to_audio_codec("ac3"));
        // Codecs ffmpeg only decodes (so a source can't be transcoded *to* them).
        assert!(!s.can_encode_to_audio_codec("dts"));
        assert!(!s.can_encode_to_audio_codec("truehd"));
        // Image subtitles can't be extracted to text; text subs can.
        assert!(s.can_extract_subtitles("subrip"));
        assert!(!s.can_extract_subtitles("hdmv_pgs_subtitle"));
    }
}
