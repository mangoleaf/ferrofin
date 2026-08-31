//! `MediaInfoController` — playback-info resolution.
//!
//! Ports `GET`/`POST /Items/{itemId}/PlaybackInfo`: resolves the item's playable
//! [`MediaSourceInfo`](ferrofin_model::dto::MediaSourceInfo)s for the requesting
//! user via the [`MediaSourceManager`](ferrofin_traits::library::MediaSourceManager)
//! and returns them in a [`PlaybackInfoResponse`]. The `POST` body (a device
//! profile + stream selections) is accepted and ignored for now; both verbs
//! share one handler, matching Jellyfin's two actions.

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::data::MediaStreamProtocol;
use ferrofin_model::dlna::{
    DeviceProfile, DlnaProfileType, MediaOptions, StreamBuilder, TranscoderSupport,
};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::media_info::{LiveStreamRequest, LiveStreamResponse, PlaybackInfoResponse};
use ferrofin_model::secret::Secret;
use ferrofin_model::session::PlayMethod;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::handlers::item_update::opt_i32;
use crate::handlers::items::{effective_user_id, resolve_user, user_uuid};
use crate::state::AppState;

/// The server's transcode capabilities, fed to the [`StreamBuilder`] so it knows
/// which codecs a non-direct-play source can be transcoded to. Reflects the codecs
/// the ffmpeg build ships with (matching Jellyfin's default encoder set).
struct FerrofinTranscoderSupport;

impl TranscoderSupport for FerrofinTranscoderSupport {
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
///
/// Jellyfin clients send these PascalCase (the C# model binder is
/// case-insensitive; axum's `Query` is not), so each field carries a PascalCase
/// alias alongside the camelCase rename.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaybackInfoQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        alias = "UserId",
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// The requested audio stream index override.
    #[serde(default, alias = "AudioStreamIndex")]
    audio_stream_index: Option<i32>,
    /// The requested subtitle stream index override (`-1` = none).
    #[serde(default, alias = "SubtitleStreamIndex")]
    subtitle_stream_index: Option<i32>,
    /// Whether direct play is permitted (default true).
    #[serde(default, alias = "EnableDirectPlay")]
    enable_direct_play: Option<bool>,
    /// Whether direct stream is permitted (default true).
    #[serde(default, alias = "EnableDirectStream")]
    enable_direct_stream: Option<bool>,
    /// Whether transcoding is permitted (default true).
    #[serde(default, alias = "EnableTranscoding")]
    enable_transcoding: Option<bool>,
    /// Whether `-c:v copy` is permitted in a transcode (default true).
    #[serde(default, alias = "AllowVideoStreamCopy")]
    allow_video_stream_copy: Option<bool>,
    /// Whether `-c:a copy` is permitted in a transcode (default true).
    #[serde(default, alias = "AllowAudioStreamCopy")]
    allow_audio_stream_copy: Option<bool>,
}

/// The resolved playback capability flags (query wins over the posted body,
/// both default to permitted — the C# `?? true` pattern).
///
/// Port of the `EnableDirectPlay`/`EnableDirectStream`/`EnableTranscoding`/
/// `AllowVideoStreamCopy`/`AllowAudioStreamCopy` parameters of
/// `MediaInfoController.GetPostedPlaybackInfo`.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // a faithful mirror of the C# flag set
struct PlaybackFlags {
    enable_direct_play: bool,
    enable_direct_stream: bool,
    enable_transcoding: bool,
    allow_video_stream_copy: bool,
    allow_audio_stream_copy: bool,
}

impl Default for PlaybackFlags {
    fn default() -> Self {
        Self {
            enable_direct_play: true,
            enable_direct_stream: true,
            enable_transcoding: true,
            allow_video_stream_copy: true,
            allow_audio_stream_copy: true,
        }
    }
}

/// Resolves the playback info for `item_id` and the effective user.
///
/// Shared by the `GET` and `POST` handlers. `allow_media_probe` and
/// `enable_path_substitution` mirror the C# defaults for the basic path.
#[allow(clippy::too_many_arguments)] // mirrors the C# parameter surface
async fn playback_info(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    item_id: Uuid,
    user_id: Option<Uuid>,
    profile: Option<&DeviceProfile>,
    max_streaming_bitrate: Option<i32>,
    stream_selection: StreamSelection,
    flags: PlaybackFlags,
) -> Result<PlaybackInfoResponse, ApiError> {
    let user = resolve_user(state, auth, user_id).await?;
    let resolved_user_id = user_uuid(&user)?;
    let mut media_sources = state
        .media_sources
        .get_playback_media_sources(item_id, resolved_user_id, true, true)
        .await?;

    // Stamp `HasSegments` from the media-segment store (upstream
    // MediaSourceManager does this via IMediaSegmentManager). jellyfin-web's
    // segment manager checks this flag at playback start and never even
    // fetches `/MediaSegments/{id}` without it — so the skip-intro button
    // can't appear regardless of what detection stored.
    let has_segments = state
        .media_segments
        .has_segments(item_id)
        .await
        .unwrap_or(false);
    for source in &mut media_sources {
        source.has_segments = has_segments;
    }

    // The client threads this id through every playback-progress report; C#
    // mints a fresh GUID per PlaybackInfo call, so a null here breaks reporting.
    // Minted before the decision loop so the metrics row can be keyed by it.
    // `MediaInfoHelper.cs:142` formats it `ToString("N")` — 32 hex digits, no
    // hyphens — which is the shape clients echo back.
    let play_session_id = Uuid::new_v4().simple().to_string();

    // When the client posts its device profile, decide per source whether it can
    // direct-play or must transcode (e.g. h264 video but AC3 audio a browser can't
    // decode). Without this every source claimed direct-play, so incompatible-audio
    // files played video-only. No profile (the GET form) keeps the static sources.
    if let Some(profile) = profile {
        let mut first_decision = None;
        for source in &mut media_sources {
            let decision = apply_stream_decision(
                source,
                profile,
                item_id,
                max_streaming_bitrate,
                auth.token.as_ref().map(Secret::expose),
                auth.device_id.as_deref(),
                stream_selection,
                &play_session_id,
                flags,
            );
            if first_decision.is_none() {
                first_decision = decision.map(|d| (d, source.clone()));
            }
        }
        // Record the decision for the metrics track (Track A). Clients play the
        // first/default source; multi-version items are the rare exception, so
        // one row per play session (not per source) keeps the data readable.
        if let (Some((decision, source)), Some(metrics)) =
            (first_decision, state.playback_metrics.as_ref())
        {
            let record = ferrofin_traits::metrics::PlaybackDecision {
                play_session_id: play_session_id.clone(),
                item_id,
                user_id: resolved_user_id,
                client: auth.client.clone(),
                device_id: auth.device_id.clone(),
                play_method: decision.play_method.to_owned(),
                transcode_reasons: decision.transcode_reasons,
                container: source.container.clone(),
                video_codec: source.video_stream().and_then(|s| s.codec.clone()),
                audio_codec: source
                    .get_default_audio_stream(source.default_audio_stream_index)
                    .and_then(|s| s.codec.clone()),
                target_container: decision.target_container,
                target_video_codec: decision.target_video_codec,
                target_audio_codec: decision.target_audio_codec,
            };
            // Observability only — never fail the request over it.
            let _ = metrics.record_decision(&record).await;
        }
    }

    Ok(PlaybackInfoResponse {
        media_sources,
        play_session_id: Some(play_session_id),
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

/// The client's requested stream-index overrides, threaded from the
/// PlaybackInfo query/body into the [`StreamBuilder`]'s [`MediaOptions`].
#[derive(Debug, Clone, Copy, Default)]
struct StreamSelection {
    /// The requested audio stream index.
    audio_stream_index: Option<i32>,
    /// The requested subtitle stream index (`-1` = none selected).
    subtitle_stream_index: Option<i32>,
}

/// The play decision summary for one source, fed to the metrics recorder
/// (`PlaybackSessions`): the final method plus the negotiated targets.
struct StreamDecision {
    /// `DirectPlay` | `Transcode` (what the client will actually do).
    play_method: &'static str,
    /// Comma-joined `TranscodeReason` names; empty for direct play.
    transcode_reasons: String,
    /// Negotiated target container (transcode only).
    target_container: Option<String>,
    /// Negotiated target video codec (transcode only).
    target_video_codec: Option<String>,
    /// Negotiated target audio codec (transcode only).
    target_audio_codec: Option<String>,
}

/// Runs the DLNA [`StreamBuilder`] for one media source against the client's
/// profile and stamps the resulting play decision onto it: direct-play stays as-is,
/// otherwise `SupportsDirectPlay` is cleared and a `TranscodingUrl` (the HLS master
/// / remux stream, with the negotiated codecs) is set so the client transcodes.
/// Returns the decision summary for the metrics recorder ([`None`] when the
/// builder produced no stream).
#[allow(clippy::too_many_arguments)] // mirrors the C# parameter surface
fn apply_stream_decision(
    source: &mut MediaSourceInfo,
    profile: &DeviceProfile,
    item_id: Uuid,
    max_streaming_bitrate: Option<i32>,
    token: Option<&str>,
    device_id: Option<&str>,
    stream_selection: StreamSelection,
    play_session_id: &str,
    flags: PlaybackFlags,
) -> Option<StreamDecision> {
    let mut options = MediaOptions::new(profile.clone());
    options.item_id = item_id;
    options.media_source_id.clone_from(&source.id);
    options.device_id = device_id.map(str::to_owned);
    options.max_bitrate = max_streaming_bitrate;
    // The client's capability veto (EnableDirectPlay/EnableDirectStream): the
    // builder never picks a vetoed method.
    options.enable_direct_play = flags.enable_direct_play;
    options.enable_direct_stream = flags.enable_direct_stream;
    // The client's explicit stream picks (a `-1` subtitle index means "none" —
    // C# treats negatives as unset when resolving the stream).
    options.audio_stream_index = stream_selection.audio_stream_index;
    options.subtitle_stream_index = stream_selection.subtitle_stream_index.filter(|&i| i >= 0);
    options.media_sources = vec![source.clone()];

    let support = FerrofinTranscoderSupport;
    let stream = StreamBuilder::new(&support).get_optimal_video_stream(&options)?;

    if stream.play_method == PlayMethod::DirectPlay {
        source.supports_direct_play = true;
        apply_subtitle_delivery(source, &stream, &support, token);
        return Some(StreamDecision {
            play_method: "DirectPlay",
            transcode_reasons: String::new(),
            target_container: None,
            target_video_codec: None,
            target_audio_codec: None,
        });
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

    // The pin above invalidates a subtitle delivery method decided under the
    // builder's DirectStream context (e.g. `Embed` — impossible in an HLS ts
    // segment). Recompute it for the pinned Transcode+HLS state, exactly as the
    // builder's own transcode branch would, so the `SubtitleMethod` baked into
    // the transcoding URL agrees with the per-stream `DeliveryMethod` DTOs from
    // `apply_subtitle_delivery`. When they disagreed the transcode burned the
    // track in while the client also rendered the external VTT it was promised
    // — the same subtitle twice on screen.
    if let Some(index) = stream.subtitle_stream_index
        && let Some(sub) = source
            .media_streams
            .iter()
            .find(|s| {
                s.stream_type == ferrofin_model::entities::MediaStreamType::Subtitle
                    && s.index == index
            })
            .cloned()
    {
        let subtitle_profile = StreamBuilder::get_subtitle_profile(
            source,
            &sub,
            &profile.subtitle_profiles,
            PlayMethod::Transcode,
            &support,
            Some(container.as_str()),
            Some(MediaStreamProtocol::hls),
        );
        stream.subtitle_delivery_method = subtitle_profile.method;
        stream.subtitle_format.clone_from(&subtitle_profile.format);
        stream.subtitle_codecs = subtitle_profile.format.clone().into_iter().collect();
    }

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
    // A transcoding veto (EnableTranscoding=false) leaves the source with no
    // playable method — the C# behaviour: the client shows its "can't play"
    // error rather than silently transcoding.
    if !flags.enable_transcoding {
        source.supports_transcoding = false;
        apply_subtitle_delivery(source, &stream, &support, token);
        return Some(StreamDecision {
            play_method: "Transcode",
            transcode_reasons: ferrofin_model::session::transcode_reasons_unique_names(
                stream.transcode_reasons,
            )
            .join(","),
            target_container: Some(container),
            target_video_codec: stream.video_codecs.first().cloned(),
            target_audio_codec: stream.audio_codecs.first().cloned(),
        });
    }
    source.supports_transcoding = true;
    // The playback-session id rides the transcode URL so the spawned job is
    // psid-addressable (`DELETE /Videos/ActiveEncodings?playSessionId=…`).
    stream.play_session_id = Some(play_session_id.to_owned());
    let mut transcoding_url = stream.to_url(None, token, None);
    // The C# controller appends the copy vetoes onto the negotiated URL; the
    // transcoder reads them back as `allowVideoStreamCopy`/`allowAudioStreamCopy`.
    if !flags.allow_video_stream_copy {
        transcoding_url.push_str("&AllowVideoStreamCopy=false");
    }
    if !flags.allow_audio_stream_copy {
        transcoding_url.push_str("&AllowAudioStreamCopy=false");
    }
    source.transcoding_url = Some(transcoding_url);
    source.transcoding_sub_protocol = MediaStreamProtocol::hls;
    source.transcoding_container = Some(container.clone());
    apply_subtitle_delivery(source, &stream, &support, token);
    Some(StreamDecision {
        play_method: "Transcode",
        transcode_reasons: ferrofin_model::session::transcode_reasons_unique_names(
            stream.transcode_reasons,
        )
        .join(","),
        target_container: Some(container),
        target_video_codec: stream.video_codecs.first().cloned(),
        target_audio_codec: stream.audio_codecs.first().cloned(),
    })
}

/// Populates each subtitle stream's `DeliveryMethod`/`DeliveryUrl` on the source
/// (Jellyfin's `StreamInfo.GetSubtitleProfiles`). Without it every subtitle has
/// no delivery method, so the client can't fetch (text → external VTT) or
/// request burn-in (image subs like DVDSUB/PGS → `Encode`), and a selected
/// subtitle simply never renders.
fn apply_subtitle_delivery(
    source: &mut MediaSourceInfo,
    stream: &ferrofin_model::dlna::StreamInfo,
    support: &FerrofinTranscoderSupport,
    token: Option<&str>,
) {
    // Relative URLs (empty base), matching the transcoding URL; all subtitles
    // (not just a selected one) so the client knows how to handle each — but ONE
    // resolved profile per stream (`enable_all_profiles=false`, the C#
    // `SetDeviceSpecificData` overload). With `true` this yields an entry per
    // device subtitle profile and the loop below overwrites the stream's
    // delivery method with each in turn, so the last profile's Encode fallback
    // clobbered the External+conversion match and no subtitle ever rendered.
    let infos = stream.get_subtitle_profiles(support, false, false, "", token);
    for info in &infos {
        if let Some(sub) = source.media_streams.iter_mut().find(|s| {
            s.stream_type == ferrofin_model::entities::MediaStreamType::Subtitle
                && s.index == info.index
        }) {
            sub.delivery_method = Some(info.delivery_method);
            sub.delivery_url.clone_from(&info.url);
            sub.is_external_url = Some(info.is_external_url);
        }
    }
}

/// `GET /Items/{itemId}/PlaybackInfo` — playback info for the item.
///
/// Port of `MediaInfoController.GetPlaybackInfo`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/PlaybackInfo",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 200, description = "Playback info returned", body = PlaybackInfoResponse)),
    tag = "ferrofin"
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
        playback_info(
            &state,
            &auth,
            item_id,
            query.user_id,
            None,
            None,
            StreamSelection::default(),
            flags_from(&query, None),
        )
        .await?,
    ))
}

/// The `POST /Items/{itemId}/PlaybackInfo` body — the client's device profile,
/// the streaming limits, and the stream-index selections used to negotiate the
/// play method. Query parameters take precedence over body fields (the C#
/// `?? playbackInfoDto?.Field` pattern).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlaybackInfoBody {
    #[serde(default)]
    device_profile: Option<DeviceProfile>,
    // The numeric fields use the lenient number-or-string deserializer: the C#
    // binder runs with `JsonNumberHandling.AllowReadingFromString`, and clients
    // (jellyfin-web track pickers) really do post `"AudioStreamIndex": "1"` — a
    // strict i32 turns the whole request into a 422.
    #[serde(default, deserialize_with = "opt_i32")]
    max_streaming_bitrate: Option<i32>,
    #[serde(default, deserialize_with = "opt_i32")]
    audio_stream_index: Option<i32>,
    #[serde(default, deserialize_with = "opt_i32")]
    subtitle_stream_index: Option<i32>,
    #[serde(default)]
    enable_direct_play: Option<bool>,
    #[serde(default)]
    enable_direct_stream: Option<bool>,
    #[serde(default)]
    enable_transcoding: Option<bool>,
    #[serde(default)]
    allow_video_stream_copy: Option<bool>,
    #[serde(default)]
    allow_audio_stream_copy: Option<bool>,
}

/// Resolves the capability flags: query wins over body, both default to
/// permitted (the C# `request.X ?? playbackInfoDto?.X ?? true`).
fn flags_from(query: &PlaybackInfoQuery, body: Option<&PlaybackInfoBody>) -> PlaybackFlags {
    let pick = |q: Option<bool>, b: Option<bool>| q.or(b).unwrap_or(true);
    PlaybackFlags {
        enable_direct_play: pick(
            query.enable_direct_play,
            body.and_then(|b| b.enable_direct_play),
        ),
        enable_direct_stream: pick(
            query.enable_direct_stream,
            body.and_then(|b| b.enable_direct_stream),
        ),
        enable_transcoding: pick(
            query.enable_transcoding,
            body.and_then(|b| b.enable_transcoding),
        ),
        allow_video_stream_copy: pick(
            query.allow_video_stream_copy,
            body.and_then(|b| b.allow_video_stream_copy),
        ),
        allow_audio_stream_copy: pick(
            query.allow_audio_stream_copy,
            body.and_then(|b| b.allow_audio_stream_copy),
        ),
    }
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
    tag = "ferrofin"
)]
async fn post_playback_info(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<PlaybackInfoQuery>,
    body: Option<JsonBody<PlaybackInfoBody>>,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    let body = body.map(|JsonBody(b)| b).unwrap_or_default();
    let stream_selection = StreamSelection {
        audio_stream_index: query.audio_stream_index.or(body.audio_stream_index),
        subtitle_stream_index: query.subtitle_stream_index.or(body.subtitle_stream_index),
    };
    Ok(Json(
        playback_info(
            &state,
            &auth,
            item_id,
            query.user_id,
            body.device_profile.as_ref(),
            body.max_streaming_bitrate,
            stream_selection,
            flags_from(&query, Some(&body)),
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
    /// Whether direct play is enabled. Default `true`.
    #[serde(default)]
    enable_direct_play: Option<bool>,
    /// Whether direct stream is enabled. Default `true`.
    #[serde(default)]
    enable_direct_stream: Option<bool>,
    /// Whether subtitles are always burned in when transcoding. Default `false`.
    #[serde(default)]
    always_burn_in_subtitle_when_transcoding: Option<bool>,
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    item_id: Option<Uuid>,
}

/// Reads an optional `i64` that a client may have posted as a JSON string.
///
/// The `i64` twin of [`opt_i32`]: the C# binder runs with
/// `JsonNumberHandling.AllowReadingFromString`, and a client that quotes
/// `StartTimeTicks` must not turn the whole request into a `422`.
///
/// # Errors
///
/// Fails when the value is neither a number, a numeric string, nor `null`.
fn opt_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Number(i64),
        Text(String),
        Null,
    }
    match <Lenient as serde::Deserialize>::deserialize(d)? {
        Lenient::Number(n) => Ok(Some(n)),
        Lenient::Text(s) if s.trim().is_empty() => Ok(None),
        Lenient::Text(s) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("expected an integer, got {s:?}"))),
        Lenient::Null => Ok(None),
    }
}

/// The posted `OpenLiveStreamDto` body.
///
/// Every scalar the query carries may instead arrive here — jellyfin-web and the
/// Android clients POST the whole request as a body — and the query wins where
/// both are present (C# `openToken ?? openLiveStreamDto?.OpenToken`). The device
/// profile it may also carry is not yet honoured.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct OpenLiveStreamDto {
    /// The open token identifying the source to open.
    open_token: Option<String>,
    /// The target user.
    user_id: Option<Uuid>,
    /// The play session id.
    play_session_id: Option<String>,
    /// The maximum streaming bitrate.
    #[serde(deserialize_with = "opt_i32")]
    max_streaming_bitrate: Option<i32>,
    /// The start time in ticks.
    #[serde(deserialize_with = "opt_i64")]
    start_time_ticks: Option<i64>,
    /// The audio stream index.
    #[serde(deserialize_with = "opt_i32")]
    audio_stream_index: Option<i32>,
    /// The subtitle stream index.
    #[serde(deserialize_with = "opt_i32")]
    subtitle_stream_index: Option<i32>,
    /// The maximum number of audio channels.
    #[serde(deserialize_with = "opt_i32")]
    max_audio_channels: Option<i32>,
    /// The item id whose source is opened.
    item_id: Option<Uuid>,
    /// Whether direct play is enabled.
    enable_direct_play: Option<bool>,
    /// Whether direct stream is enabled.
    enable_direct_stream: Option<bool>,
    /// Whether subtitles are always burned in when transcoding.
    always_burn_in_subtitle_when_transcoding: Option<bool>,
    /// The protocols the client will direct-play.
    direct_play_protocols: Option<Vec<ferrofin_model::media_info::MediaProtocol>>,
}

/// `POST /LiveStreams/Open` — open a media source and return its live stream.
///
/// Port of `MediaInfoController.OpenLiveStream`: the query and the posted
/// [`OpenLiveStreamDto`] are folded together (query wins) into a
/// [`LiveStreamRequest`] and handed to
/// [`MediaSourceManager::open_live_stream`](ferrofin_traits::library::MediaSourceManager::open_live_stream),
/// which redeems the open token — for a Live TV channel that opens the tuner —
/// probes the result and registers it in the open-stream table. The posted
/// device profile is not yet honoured.
#[utoipa::path(
    post,
    path = "/LiveStreams/Open",
    responses((status = 200, description = "Media source opened", body = LiveStreamResponse)),
    tag = "ferrofin"
)]
async fn open_live_stream(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<OpenLiveStreamQuery>,
    body: Option<JsonBody<OpenLiveStreamDto>>,
) -> Result<Json<LiveStreamResponse>, ApiError> {
    let dto = body.map(|JsonBody(dto)| dto).unwrap_or_default();
    // C# `userId ??= openLiveStreamDto?.UserId; userId =
    // RequestHelpers.GetUserId(User, userId)`: the query id wins over the body
    // id, and naming another user requires the administrator role. The resolved
    // id is what the stream is opened AS, so an ungated one opened a live
    // stream — and a tuner — under another account's identity.
    let effective = effective_user_id(&state, &auth, query.user_id.or(dto.user_id)).await?;
    let request = live_stream_request(query, dto, effective);
    let media_source = state.media_sources.open_live_stream(&request).await?;
    Ok(Json(LiveStreamResponse::new(media_source)))
}

/// Folds the query and the posted body into one [`LiveStreamRequest`].
///
/// Port of the `?? openLiveStreamDto?.Field` chain in
/// `MediaInfoController.OpenLiveStream`: the query wins, the body fills the
/// gaps, and the documented defaults (direct play and direct stream on,
/// burn-in off, HTTP the only direct-play protocol) apply last.
///
/// `effective_user` is the id the caller already resolved through
/// [`effective_user_id`] — the `userId`/body-`UserId` pair folded and then run
/// through the administrator gate — so the two raw ids are deliberately not
/// consulted again here.
fn live_stream_request(
    query: OpenLiveStreamQuery,
    dto: OpenLiveStreamDto,
    effective_user: Uuid,
) -> LiveStreamRequest {
    LiveStreamRequest {
        open_token: query.open_token.or(dto.open_token),
        // Already resolved through `RequestHelpers.GetUserId` by the caller.
        user_id: effective_user,
        play_session_id: query.play_session_id.or(dto.play_session_id),
        max_streaming_bitrate: query.max_streaming_bitrate.or(dto.max_streaming_bitrate),
        start_time_ticks: query.start_time_ticks.or(dto.start_time_ticks),
        audio_stream_index: query.audio_stream_index.or(dto.audio_stream_index),
        subtitle_stream_index: query.subtitle_stream_index.or(dto.subtitle_stream_index),
        max_audio_channels: query.max_audio_channels.or(dto.max_audio_channels),
        item_id: query.item_id.or(dto.item_id).unwrap_or_else(Uuid::nil),
        enable_direct_play: query
            .enable_direct_play
            .or(dto.enable_direct_play)
            .unwrap_or(true),
        enable_direct_stream: query
            .enable_direct_stream
            .or(dto.enable_direct_stream)
            .unwrap_or(true),
        always_burn_in_subtitle_when_transcoding: query
            .always_burn_in_subtitle_when_transcoding
            .or(dto.always_burn_in_subtitle_when_transcoding)
            .unwrap_or(false),
        direct_play_protocols: dto
            .direct_play_protocols
            .unwrap_or_else(|| vec![ferrofin_model::media_info::MediaProtocol::Http]),
    }
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
    tag = "ferrofin"
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
    tag = "ferrofin"
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
        FerrofinTranscoderSupport, hls_segment_container, hls_transcoding_max_audio_channels,
    };
    use ferrofin_model::data::MediaStreamProtocol;
    use ferrofin_model::dlna::{
        DeviceProfile, DlnaProfileType, TranscoderSupport, TranscodingProfile,
    };
    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_model::entities_media::MediaStream;

    #[test]
    fn open_live_stream_reads_the_posted_body_and_lets_the_query_win() {
        // The shape jellyfin-web (and the parity harness) POSTs: everything in
        // the body, nothing in the query. Before this was bound, the open token
        // never reached the manager and a Live TV channel could not be opened.
        let dto: super::OpenLiveStreamDto = serde_json::from_str(
            r#"{"OpenToken":"prov_LiveTvChannel_abc_src","UserId":"85c9c1a0f0b74a1b8c4d9e2f3a4b5c6d",
                "ItemId":"11111111222233334444555566667777","PlaySessionId":"parity-livetv",
                "EnableDirectPlay":true,"EnableDirectStream":false,
                "DeviceProfile":{"Name":"ignored"}}"#,
        )
        .expect("body parses");
        // The body's `UserId` is folded in and gated by the handler (C# does the
        // same, `userId ??= dto.UserId` then `RequestHelpers.GetUserId`), so the
        // resolved id is what this function is handed.
        let body_user = uuid::Uuid::parse_str("85c9c1a0f0b74a1b8c4d9e2f3a4b5c6d").expect("guid");
        assert_eq!(dto.user_id, Some(body_user));
        let request =
            super::live_stream_request(super::OpenLiveStreamQuery::default(), dto, body_user);
        assert_eq!(
            request.open_token.as_deref(),
            Some("prov_LiveTvChannel_abc_src")
        );
        assert_eq!(request.play_session_id.as_deref(), Some("parity-livetv"));
        assert_eq!(request.user_id, body_user);
        assert_eq!(
            request.item_id,
            uuid::Uuid::parse_str("11111111222233334444555566667777").expect("guid")
        );
        assert!(request.enable_direct_play);
        assert!(!request.enable_direct_stream);
        assert!(!request.always_burn_in_subtitle_when_transcoding);
        assert_eq!(
            request.direct_play_protocols,
            vec![ferrofin_model::media_info::MediaProtocol::Http]
        );

        // The query wins over the body wherever both speak.
        let query = super::OpenLiveStreamQuery {
            open_token: Some("from-query".to_owned()),
            enable_direct_stream: Some(true),
            ..super::OpenLiveStreamQuery::default()
        };
        let dto = super::OpenLiveStreamDto {
            open_token: Some("from-body".to_owned()),
            enable_direct_stream: Some(false),
            ..super::OpenLiveStreamDto::default()
        };
        let user = uuid::Uuid::from_u128(9);
        let request = super::live_stream_request(query, dto, user);
        assert_eq!(request.open_token.as_deref(), Some("from-query"));
        assert!(request.enable_direct_stream);
        // The resolved id passes through untouched.
        assert_eq!(request.user_id, user);
    }

    #[test]
    fn playback_info_body_accepts_stringly_numbers() {
        // jellyfin-web posts stream indexes as strings ("1", "-1"); the C# binder
        // accepts them (AllowReadingFromString) — a strict i32 made the whole
        // PlaybackInfo request a 422.
        let body: super::PlaybackInfoBody = serde_json::from_str(
            r#"{"AudioStreamIndex":"1","SubtitleStreamIndex":"-1","MaxStreamingBitrate":"140000000"}"#,
        )
        .expect("lenient parse");
        assert_eq!(body.audio_stream_index, Some(1));
        assert_eq!(body.subtitle_stream_index, Some(-1));
        assert_eq!(body.max_streaming_bitrate, Some(140_000_000));
    }

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
    #[allow(clippy::too_many_lines)] // one long fixture, one scenario
    fn pinned_transcode_subtitle_method_agrees_between_url_and_dto() {
        // The double-subtitle regression: the builder decides DirectStream and,
        // in that context, an Embed subtitle delivery. `apply_stream_decision`
        // pins the result to Transcode+HLS — where embedding in a ts segment is
        // impossible — so it must recompute the delivery. With the stale Embed
        // the transcoding URL carried `SubtitleStreamIndex&SubtitleMethod=Embed`
        // (→ the planner burned the track in) while the per-stream DTO promised
        // External with a DeliveryUrl (→ the client overlaid the VTT): the same
        // subtitle rendered twice on screen.
        use ferrofin_model::dlna::{DirectPlayProfile, SubtitleDeliveryMethod, SubtitleProfile};
        use uuid::Uuid;

        let mut source = MediaSourceInfo {
            id: Some("src1".to_owned()),
            path: Some("/media/movie.mkv".to_owned()),
            container: Some("mkv".to_owned()),
            bitrate: Some(5_000_000),
            media_streams: vec![
                MediaStream {
                    codec: Some("h264".to_owned()),
                    stream_type: MediaStreamType::Video,
                    index: 0,
                    ..MediaStream::default()
                },
                MediaStream {
                    codec: Some("aac".to_owned()),
                    stream_type: MediaStreamType::Audio,
                    index: 1,
                    ..MediaStream::default()
                },
                MediaStream {
                    codec: Some("subrip".to_owned()),
                    stream_type: MediaStreamType::Subtitle,
                    index: 2,
                    // As probing sets it for text subs — required for the
                    // External srt→vtt conversion match.
                    supports_external_stream: true,
                    ..MediaStream::default()
                },
            ],
            ..MediaSourceInfo::default()
        };
        let profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                container: "mkv".to_owned(),
                video_codec: Some("h264".to_owned()),
                audio_codec: Some("aac".to_owned()),
                profile_type: DlnaProfileType::Video,
            }],
            transcoding_profiles: vec![TranscodingProfile {
                container: "ts".to_owned(),
                profile_type: DlnaProfileType::Video,
                protocol: MediaStreamProtocol::hls,
                video_codec: "h264".to_owned(),
                audio_codec: "aac".to_owned(),
                ..TranscodingProfile::default()
            }],
            subtitle_profiles: vec![
                // Matches under the DirectStream context (embedded subrip in mkv)…
                SubtitleProfile {
                    format: Some("subrip".to_owned()),
                    method: SubtitleDeliveryMethod::Embed,
                    container: Some("mkv".to_owned()),
                    ..SubtitleProfile::default()
                },
                // …but a ts HLS transcode can only deliver it externally.
                SubtitleProfile {
                    format: Some("vtt".to_owned()),
                    method: SubtitleDeliveryMethod::External,
                    ..SubtitleProfile::default()
                },
            ],
            ..DeviceProfile::default()
        };
        // DirectPlay vetoed so the builder lands on DirectStream, which the
        // handler pins to Transcode.
        let flags = super::PlaybackFlags {
            enable_direct_play: false,
            ..super::PlaybackFlags::default()
        };
        let decision = super::apply_stream_decision(
            &mut source,
            &profile,
            Uuid::from_u128(0xBEEF),
            Some(20_000_000),
            None,
            None,
            super::StreamSelection {
                audio_stream_index: None,
                subtitle_stream_index: Some(2),
            },
            "ps1",
            flags,
        )
        .expect("a decision");
        assert_eq!(decision.play_method, "Transcode");

        let url = source.transcoding_url.as_deref().expect("a transcode URL");
        let sub = source
            .media_streams
            .iter()
            .find(|s| s.index == 2)
            .expect("subtitle stream");
        assert_eq!(
            sub.delivery_method,
            Some(SubtitleDeliveryMethod::External),
            "ts HLS transcode delivers text subs externally"
        );
        assert!(
            sub.delivery_url.is_some(),
            "External delivery needs a DeliveryUrl"
        );
        // External delivery ⇒ the transcode must NOT be asked to burn it in.
        assert!(
            !url.contains("SubtitleStreamIndex"),
            "URL must not carry the subtitle index when the client renders it: {url}"
        );
        assert!(
            !url.contains("SubtitleMethod"),
            "no stale SubtitleMethod on the URL: {url}"
        );
    }

    #[test]
    fn transcoder_support_reports_the_ffmpeg_audio_codecs() {
        let s = FerrofinTranscoderSupport;
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
