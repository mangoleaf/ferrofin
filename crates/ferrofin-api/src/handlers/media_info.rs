//! `MediaInfoController` — playback-info resolution.
//!
//! Ports `GET`/`POST /Items/{itemId}/PlaybackInfo`: resolves the item's playable
//! [`MediaSourceInfo`](ferrofin_model::dto::MediaSourceInfo)s for the requesting
//! user via the [`MediaSourceManager`](ferrofin_traits::library::MediaSourceManager)
//! and returns them in a [`PlaybackInfoResponse`]. The `POST` form also takes
//! the client's device profile and stream picks (query or body) and negotiates
//! each source's play method against them; the `GET` form returns the static
//! sources, matching Jellyfin's two actions.

use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::data::MediaStreamProtocol;
use ferrofin_model::dlna::{
    DeviceProfile, DlnaProfileType, MediaOptions, StreamBuilder, TranscoderSupport,
};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::media_info::{
    LiveStreamRequest, LiveStreamResponse, MediaProtocol, PlaybackInfoResponse,
};
use ferrofin_model::secret::Secret;
use ferrofin_model::session::PlayMethod;
use ferrofin_traits::net::RequestContext;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::{JsonBody, Query};
use crate::handlers::items::{
    effective_user_id, require_visible_item, resolve_user_opt, user_uuid,
};
use crate::state::AppState;

/// The server's transcode capabilities the [`StreamBuilder`] asks, for one
/// request: `MediaEncoder` as `ITranscoderSupport` (`MediaEncoder.cs`, v12.2).
struct FerrofinTranscoderSupport<'a> {
    /// The probed ffmpeg's encoders (`SupportsEncoder`).
    encoders: &'a dyn ferrofin_traits::media_encoding::EncoderSupport,
    /// `EncodingOptions.EnableSubtitleExtraction`, read for this request.
    enable_subtitle_extraction: bool,
}

impl<'a> FerrofinTranscoderSupport<'a> {
    /// The capabilities `state` has now: its probed encoders and the current
    /// encoding options' subtitle extraction setting.
    async fn current(state: &'a AppState) -> Result<Self, ApiError> {
        Ok(Self {
            encoders: state.encoder_support.as_ref(),
            enable_subtitle_extraction: state
                .config
                .get_encoding_options()
                .await?
                .enable_subtitle_extraction,
        })
    }
}

impl TranscoderSupport for FerrofinTranscoderSupport<'_> {
    /// `CanEncodeToAudioCodec`: opus and mp3 are asked as their ffmpeg
    /// encoders, `libopus` and `libmp3lame`; every other codec by its name.
    fn can_encode_to_audio_codec(&self, codec: &str) -> bool {
        let encoder = if codec.eq_ignore_ascii_case("opus") {
            "libopus"
        } else if codec.eq_ignore_ascii_case("mp3") {
            "libmp3lame"
        } else {
            codec
        };
        self.encoders.supports_encoder(encoder)
    }
    /// `CanEncodeToSubtitleCodec`: always, as upstream answers.
    fn can_encode_to_subtitle_codec(&self, _codec: &str) -> bool {
        true
    }
    /// `CanExtractSubtitles`: the `EnableSubtitleExtraction` setting.
    fn can_extract_subtitles(&self, _codec: &str) -> bool {
        self.enable_subtitle_extraction
    }
}

/// Query parameters for the playback-info endpoints.
///
/// Jellyfin clients send these PascalCase; keys bind ignoring case, as the C#
/// model binder's do ([`crate::extract::Query`]).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaybackInfoQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// The requested audio stream index override.
    #[serde(default)]
    audio_stream_index: Option<i32>,
    /// The requested subtitle stream index override (`-1` = none).
    #[serde(default)]
    subtitle_stream_index: Option<i32>,
    /// Whether direct play is permitted (default true).
    #[serde(default)]
    enable_direct_play: Option<bool>,
    /// Whether direct stream is permitted (default true).
    #[serde(default)]
    enable_direct_stream: Option<bool>,
    /// Whether transcoding is permitted (default true).
    #[serde(default)]
    enable_transcoding: Option<bool>,
    /// Whether `-c:v copy` is permitted in a transcode (default true).
    #[serde(default)]
    allow_video_stream_copy: Option<bool>,
    /// Whether `-c:a copy` is permitted in a transcode (default true).
    #[serde(default)]
    allow_audio_stream_copy: Option<bool>,
    /// The client's maximum streaming bitrate (`POST` only).
    #[serde(default)]
    max_streaming_bitrate: Option<i32>,
    /// The playback start position in ticks (`POST` only).
    #[serde(default)]
    start_time_ticks: Option<i64>,
    /// The maximum number of audio channels (`POST` only).
    #[serde(default)]
    max_audio_channels: Option<i32>,
    /// The media source (version) to play (`POST` only).
    #[serde(default)]
    media_source_id: Option<String>,
    /// An already-open live stream to play (`POST` only).
    #[serde(default)]
    live_stream_id: Option<String>,
    /// Whether to open a source that requires opening (`POST` only).
    #[serde(default)]
    auto_open_live_stream: Option<bool>,
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

/// The inputs of `MediaInfoController.GetPostedPlaybackInfo`, folded from the
/// query and the posted body (query wins). The `GET` form fills only the user
/// and the flags.
#[derive(Debug, Default)]
struct PlaybackRequest {
    /// The requested user (before the administrator gate).
    user_id: Option<Uuid>,
    /// The client's device profile; [`None`] keeps the static sources.
    profile: Option<DeviceProfile>,
    /// The client's maximum streaming bitrate.
    max_streaming_bitrate: Option<i32>,
    /// The media source (version) the client asked for.
    media_source_id: Option<String>,
    /// An already-open live stream the client asked for.
    live_stream_id: Option<String>,
    /// Whether to open the chosen source when it requires opening.
    auto_open_live_stream: bool,
    /// The stream picks and limits handed to the [`StreamBuilder`].
    selection: StreamSelection,
    /// The client's capability vetoes.
    flags: PlaybackFlags,
}

/// A non-blank optional string (C# `!string.IsNullOrWhiteSpace`).
fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

/// `MediaInfoHelper.ResolvePlaybackMediaSources`: an open live stream when
/// one is named, otherwise the item's playback sources, narrowed to the
/// requested version when the client names one.
async fn resolve_playback_media_sources(
    state: &AppState,
    item_id: Uuid,
    user_id: Uuid,
    media_source_id: Option<&str>,
    live_stream_id: Option<&str>,
) -> Result<Vec<MediaSourceInfo>, ApiError> {
    if let Some(live_stream_id) = non_blank(live_stream_id) {
        return Ok(vec![
            state.media_sources.get_live_stream(live_stream_id).await?,
        ]);
    }
    let mut sources = state
        .media_sources
        .get_playback_media_sources(item_id, user_id, true, true)
        .await?;
    if let Some(id) = non_blank(media_source_id) {
        sources.retain(|s| s.id.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(id)));
    }
    Ok(sources)
}

/// The route Live TV serves a shared stream's buffer under.
const LIVE_STREAM_FILES_ROUTE: &str = "/LiveTv/LiveStreamFiles/";

/// `MediaInfoHelper.RewritePublishedLiveStreamPath`: points an opened live
/// stream's buffer URL, built on the loopback address for the server's own
/// ffmpeg and recorder, at the URL this request reached the server by. Only
/// the response copy changes: the stream the media-source manager holds keeps
/// its local path. A smart URL that cannot be resolved leaves the path as it
/// is rather than failing an open that already holds a tuner.
async fn publish_live_stream_path(
    state: &AppState,
    client: &RequestContext,
    source: &mut MediaSourceInfo,
) {
    // Opened live streams always carry a LiveStreamId; this excludes pre-open
    // and plugin/remote sources.
    if non_blank(source.live_stream_id.as_deref()).is_none() {
        return;
    }
    let smart_api_url = match state.app_host.get_smart_api_url(client).await {
        Ok(url) => url,
        Err(err) => {
            tracing::warn!(%err, "could not resolve the request's API URL; the live stream path is left as it is");
            return;
        }
    };
    let base_url = state.app_host.base_url();
    match published_live_stream_path(
        &smart_api_url,
        source.path.as_deref(),
        source.protocol,
        &base_url,
    ) {
        Some(published) => source.path = Some(published),
        None if source.path.as_deref().is_some_and(|path| {
            path.to_ascii_lowercase()
                .contains(&LIVE_STREAM_FILES_ROUTE.to_ascii_lowercase())
        }) =>
        {
            tracing::debug!(
                media_source_id = ?source.id,
                "not rewriting the live stream path: it did not resolve under the request's \
                 smart API URL and base URL"
            );
        }
        None => {}
    }
}

/// `MediaInfoHelper.GetPublishedLiveStreamPath`: `local_path` re-based on
/// `smart_api_url`, or `None` when it is not a server-hosted HTTP
/// `[base_url]/LiveTv/LiveStreamFiles/…` URL. The query is kept and a fragment
/// dropped; `base_url` is appended to the smart URL unless its path already
/// ends with it.
fn published_live_stream_path(
    smart_api_url: &str,
    local_path: Option<&str>,
    protocol: MediaProtocol,
    base_url: &str,
) -> Option<String> {
    if protocol != MediaProtocol::Http {
        return None;
    }
    // `Uri.TryCreate(.., Absolute)`; `PathAndQuery` never carries a fragment.
    let local: axum::http::Uri = local_path?.split('#').next()?.parse().ok()?;
    local.scheme()?;
    let mut relative = local.path_and_query()?.as_str();
    if !base_url.is_empty() {
        let prefix = format!("{base_url}/");
        if !relative
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
        {
            return None;
        }
        relative = &relative[base_url.len()..];
    }
    if !relative
        .get(..LIVE_STREAM_FILES_ROUTE.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(LIVE_STREAM_FILES_ROUTE))
    {
        return None;
    }
    let mut published = smart_api_url.trim_end_matches('/').to_owned();
    if !base_url.is_empty() {
        let includes_base_url = published
            .parse::<axum::http::Uri>()
            .ok()
            .filter(|uri| uri.scheme().is_some())
            .is_some_and(|uri| {
                let path = percent_encoding::percent_decode_str(uri.path()).decode_utf8_lossy();
                let path = path.trim_end_matches('/');
                path.len()
                    .checked_sub(base_url.len())
                    .and_then(|at| path.get(at..))
                    .is_some_and(|tail| tail.eq_ignore_ascii_case(base_url))
            });
        if !includes_base_url {
            published.push_str(base_url);
        }
    }
    published.push_str(relative);
    Some(published)
}

/// `MediaInfoHelper.SortMediaSources` (the v12 form): the queried item's own
/// source first (it carries the user's resume state for that version), then
/// direct-playable files, direct-playable or -streamable sources, files, and
/// sources that fit `max_bitrate`; ties keep their order (a stable sort).
fn sort_media_sources(
    sources: &mut [MediaSourceInfo],
    max_bitrate: Option<i32>,
    preferred_item_id: Uuid,
) {
    let preferred_id =
        (!preferred_item_id.is_nil()).then(|| preferred_item_id.simple().to_string());
    sources.sort_by_key(|s| {
        let preferred = preferred_id
            .as_deref()
            .is_some_and(|p| s.id.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(p)));
        let is_file = s.protocol == MediaProtocol::File;
        let bitrate_fit = match (max_bitrate, s.bitrate) {
            (Some(max), Some(bitrate)) if bitrate <= max => 0,
            (Some(_), Some(_)) => 2,
            _ => 1,
        };
        (
            !preferred,
            !(s.supports_direct_play && is_file),
            !(s.supports_direct_play || s.supports_direct_stream),
            !is_file,
            bitrate_fit,
        )
    });
}

/// Resolves the playback info for `item_id` and the effective user.
///
/// Shared by the `GET` and `POST` handlers; port of
/// `MediaInfoController.GetPostedPlaybackInfo` from the user lookup on.
#[allow(clippy::too_many_lines)] // one upstream action, step for step
async fn playback_info(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    client: &RequestContext,
    item_id: Uuid,
    request: PlaybackRequest,
) -> Result<PlaybackInfoResponse, ApiError> {
    let user = resolve_user_opt(state, auth, request.user_id).await?;
    let item = require_visible_item(state, item_id, user.as_ref()).await?;
    let resolved_user_id = match &user {
        Some(user) => user_uuid(user)?,
        None => effective_user_id(state, auth, request.user_id).await?,
    };
    let mut media_sources = resolve_playback_media_sources(
        state,
        item_id,
        resolved_user_id,
        request.media_source_id.as_deref(),
        request.live_stream_id.as_deref(),
    )
    .await?;
    if media_sources.is_empty() {
        return Ok(PlaybackInfoResponse {
            media_sources,
            play_session_id: None,
            error_code: Some(ferrofin_model::dlna::PlaybackErrorCode::NoCompatibleStream),
        });
    }
    // `GetPlaybackInfo` rewrites its clones before any device-specific data.
    for source in &mut media_sources {
        publish_live_stream_path(state, client, source).await;
    }

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
    if let Some(profile) = &request.profile {
        let decisions = set_device_specific_data(
            state,
            auth,
            user.as_ref(),
            resolved_user_id,
            item_id,
            item.media_type.as_deref(),
            &mut media_sources,
            profile,
            &request,
            &play_session_id,
        )
        .await?;
        // Upstream sorts on the client's own limit, not the policy-capped one.
        sort_media_sources(&mut media_sources, request.max_streaming_bitrate, item_id);
        // Record the decision for the metrics track (Track A). Clients play the
        // first/default source; multi-version items are the rare exception, so
        // one row per play session (not per source) keeps the data readable.
        let first_decision = media_sources.first().and_then(|first| {
            decisions
                .into_iter()
                .find(|(id, _)| *id == first.id)
                .map(|(_, decision)| (decision, first.clone()))
        });
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

    if request.auto_open_live_stream {
        let source = match non_blank(request.media_source_id.as_deref()) {
            None => media_sources.first(),
            Some(id) => media_sources.iter().find(|s| s.id.as_deref() == Some(id)),
        };
        if let Some(source) = source
            && source.requires_opening
            && non_blank(source.live_stream_id.as_deref()).is_none()
        {
            let open = LiveStreamRequest {
                open_token: source.open_token.clone(),
                user_id: resolved_user_id,
                play_session_id: Some(play_session_id.clone()),
                max_streaming_bitrate: request.max_streaming_bitrate,
                start_time_ticks: request.selection.start_time_ticks,
                audio_stream_index: request.selection.audio_stream_index,
                subtitle_stream_index: request.selection.subtitle_stream_index,
                max_audio_channels: request.selection.max_audio_channels,
                item_id,
                enable_direct_play: request.flags.enable_direct_play,
                enable_direct_stream: request.flags.enable_direct_stream,
                always_burn_in_subtitle_when_transcoding: request
                    .selection
                    .always_burn_in_subtitle_when_transcoding,
                direct_play_protocols: vec![ferrofin_model::media_info::MediaProtocol::Http],
            };
            // The same open as `POST /LiveStreams/Open`, published the same way.
            let mut opened = state.media_sources.open_live_stream(&open).await?;
            publish_live_stream_path(state, client, &mut opened).await;
            media_sources = vec![opened];
        }
    }

    Ok(PlaybackInfoResponse {
        media_sources,
        play_session_id: Some(play_session_id),
        error_code: None,
    })
}

/// `MediaInfoHelper.SetDeviceSpecificData` over every source of an item: the
/// user's policy and bitrate limit, then one [`apply_stream_decision`] each,
/// with the track picks applied only to the version `request` names. Returns
/// each decided source's id with its decision.
#[allow(clippy::too_many_arguments)] // mirrors the C# parameter surface
async fn set_device_specific_data(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    user: Option<&ferrofin_db::entities::users::UserEntity>,
    resolved_user_id: Uuid,
    item_id: Uuid,
    media_type: Option<&str>,
    media_sources: &mut [MediaSourceInfo],
    profile: &DeviceProfile,
    request: &PlaybackRequest,
    play_session_id: &str,
) -> Result<Vec<(Option<String>, StreamDecision)>, ApiError> {
    let policy = playback_policy(state, auth, user, resolved_user_id).await?;
    // `maxStreamingBitrate ?? profile.MaxStreamingBitrate`: the remote cap
    // applies to the profile's own limit when the client sends none.
    let client_limit = request
        .max_streaming_bitrate
        .or(profile.max_streaming_bitrate);
    let max_streaming_bitrate = playback_bitrate_limit(state, auth, &policy, client_limit).await?;
    let support = FerrofinTranscoderSupport::current(state).await?;
    let mut decisions = Vec::new();
    for source in media_sources {
        // Use the item's media type rather than the presence of a video stream:
        // audio files can have cover-art video streams, and video probes may be empty.
        let is_audio = media_type.map_or_else(
            || source.video_stream().is_none(),
            |kind| kind.eq_ignore_ascii_case("Audio"),
        );
        // SetDeviceSpecificData applies the track picks only to the version
        // the client named; every other version negotiates its defaults.
        let requested = request.media_source_id.as_deref().is_some_and(|id| {
            source
                .id
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(id))
        });
        let selection = if requested {
            request.selection
        } else {
            StreamSelection {
                audio_stream_index: None,
                subtitle_stream_index: None,
                ..request.selection
            }
        };
        let decision = apply_stream_decision(
            source,
            profile,
            item_id,
            max_streaming_bitrate,
            auth.token.as_ref().map(Secret::expose),
            auth.device_id.as_deref(),
            selection,
            play_session_id,
            request.flags,
            &policy,
            is_audio,
            &support,
        );
        if let Some(decision) = decision {
            decisions.push((source.id.clone(), decision));
        }
    }
    Ok(decisions)
}

/// The media source `UniversalAudioController.GetUniversalAudioStream` acts
/// on (its `info.MediaSources[0]`), as negotiated.
pub(crate) struct UniversalAudioSource {
    /// The first-sorted source, with its negotiated `SupportsDirectPlay`/
    /// `SupportsDirectStream`, transcoding container and protocol, and
    /// `TranscodeReasons` (empty when it plays as it is).
    pub source: MediaSourceInfo,
    /// The play-session id `GetPlaybackInfo` minted for the request.
    pub play_session_id: String,
}

/// The negotiation of `UniversalAudioController.GetUniversalAudioStream`:
/// `MediaInfoHelper.GetPlaybackInfo` (the version `media_source_id` names, or
/// every version), the client's transcoding container and protocol stamped on
/// each, `SetDeviceSpecificData` against the universal `profile`, then
/// `SortMediaSources`. Returns the first source, or [`None`] when there is
/// none (upstream indexes an empty list there).
#[allow(clippy::too_many_arguments)] // mirrors the C# parameter surface
pub(crate) async fn universal_audio_source(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    item_id: Uuid,
    user_id: Option<Uuid>,
    media_source_id: Option<&str>,
    profile: &DeviceProfile,
    max_streaming_bitrate: Option<i32>,
    max_audio_channels: Option<i32>,
    transcoding_container: Option<&str>,
    transcoding_protocol: Option<MediaStreamProtocol>,
) -> Result<Option<UniversalAudioSource>, ApiError> {
    let user = resolve_user_opt(state, auth, user_id).await?;
    let item = require_visible_item(state, item_id, user.as_ref()).await?;
    let resolved_user_id = match &user {
        Some(user) => user_uuid(user)?,
        None => effective_user_id(state, auth, user_id).await?,
    };
    // Every version (the universal route takes no live stream id), narrowed
    // below rather than by the helper's exact match.
    let mut media_sources =
        resolve_playback_media_sources(state, item_id, resolved_user_id, None, None).await?;
    // The version is matched in either GUID spelling, as the static stream
    // routes match it (`MediaSourceInfo::id_matches`): the universal URL's
    // `mediaSourceId` is what a direct-play request for it would carry.
    if let Some(id) = non_blank(media_source_id) {
        media_sources.retain(|source| source.id_matches(id));
    }
    if media_sources.is_empty() {
        return Ok(None);
    }
    let play_session_id = Uuid::new_v4().simple().to_string();
    for source in &mut media_sources {
        source.transcoding_container = transcoding_container.map(str::to_owned);
        if let Some(protocol) = transcoding_protocol {
            source.transcoding_sub_protocol = protocol;
        }
    }
    // No track picks (upstream passes null indexes); its other options are
    // the request's defaults: every method allowed.
    let request = PlaybackRequest {
        user_id,
        max_streaming_bitrate,
        selection: StreamSelection {
            max_audio_channels,
            ..StreamSelection::default()
        },
        ..PlaybackRequest::default()
    };
    set_device_specific_data(
        state,
        auth,
        user.as_ref(),
        resolved_user_id,
        item_id,
        item.media_type.as_deref(),
        &mut media_sources,
        profile,
        &request,
        &play_session_id,
    )
    .await?;
    sort_media_sources(&mut media_sources, max_streaming_bitrate, item_id);
    Ok(media_sources
        .into_iter()
        .next()
        .map(|source| UniversalAudioSource {
            source,
            play_session_id,
        }))
}

/// Resolves the user policy required by upstream device-profile negotiation.
async fn playback_policy(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    user: Option<&ferrofin_db::entities::users::UserEntity>,
    resolved_user_id: Uuid,
) -> Result<std::sync::Arc<ferrofin_model::users::UserPolicy>, ApiError> {
    // SetDeviceSpecificData resolves a user even when the controller's
    // initial visibility lookup allowed a userless/API-key request.
    let user = user.ok_or_else(|| {
        if resolved_user_id.is_nil() {
            ApiError::BadRequest("Guid can't be empty".to_owned())
        } else {
            ApiError::NotFound(format!("user {resolved_user_id}"))
        }
    })?;
    if resolved_user_id == auth.user_id()
        && let Some(policy) = &auth.user_policy
    {
        return Ok(std::sync::Arc::clone(policy));
    }
    crate::handlers::users::user_policy(state, user)
        .await?
        .map(std::sync::Arc::new)
        .ok_or_else(|| ApiError::Forbidden("user policy required".to_owned()))
}

/// MediaInfoHelper.GetMaxBitrate: a positive user override replaces the server
/// cap, then the remote cap is combined with the client's request. Profile
/// defaults remain the StreamBuilder's fallback when no explicit limit exists.
async fn playback_bitrate_limit(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    policy: &ferrofin_model::users::UserPolicy,
    client_limit: Option<i32>,
) -> Result<Option<i32>, ApiError> {
    // Authorization's endpoint is normalized through KnownProxies for each
    // request; never read X-Forwarded-For directly or use the proxy's LAN IP.
    let is_local = auth
        .remote_endpoint
        .as_deref()
        .and_then(|ip| ip.parse().ok())
        .is_some_and(|ip| state.is_in_local_network(ip));
    let server_limit = if policy.remote_client_bitrate_limit > 0 || is_local {
        0
    } else {
        state
            .config
            .configuration()
            .await?
            .remote_client_bitrate_limit
    };
    Ok(remote_bitrate_limit(
        client_limit,
        policy.remote_client_bitrate_limit,
        server_limit,
        is_local,
    ))
}

fn remote_bitrate_limit(
    client: Option<i32>,
    user: i32,
    server: i32,
    is_local: bool,
) -> Option<i32> {
    let limit = if user > 0 { user } else { server };
    if is_local || limit <= 0 {
        client
    } else {
        Some(client.unwrap_or(limit).min(limit))
    }
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

/// The client's stream picks and limits, threaded from the PlaybackInfo
/// query/body into the [`StreamBuilder`]'s [`MediaOptions`] and the
/// resulting [`StreamInfo`](ferrofin_model::dlna::StreamInfo).
#[derive(Debug, Clone, Copy, Default)]
struct StreamSelection {
    /// The requested audio stream index.
    audio_stream_index: Option<i32>,
    /// The requested subtitle stream index (`-1` = none selected).
    subtitle_stream_index: Option<i32>,
    /// The client's maximum number of audio channels.
    max_audio_channels: Option<i32>,
    /// The playback start position, in ticks.
    start_time_ticks: Option<i64>,
    /// Whether a transcode always burns the selected subtitle in.
    always_burn_in_subtitle_when_transcoding: bool,
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

impl StreamDecision {
    fn transcode(stream: &ferrofin_model::dlna::StreamInfo) -> Self {
        Self {
            play_method: "Transcode",
            transcode_reasons: ferrofin_model::session::transcode_reasons_unique_names(
                stream.transcode_reasons,
            )
            .join(","),
            target_container: stream.container.clone(),
            target_video_codec: stream.video_codecs.first().cloned(),
            target_audio_codec: stream.audio_codecs.first().cloned(),
        }
    }
}

/// `MediaInfoHelper.SetDeviceSpecificData`'s choice of negotiation: an audio
/// item takes the audio one (its own direct play/remux/transcode rules and
/// `/audio/…` URLs), everything else the video one.
fn optimal_stream(
    support: &FerrofinTranscoderSupport<'_>,
    options: &MediaOptions,
    is_audio: bool,
) -> Option<ferrofin_model::dlna::StreamInfo> {
    let builder = StreamBuilder::new(support);
    if is_audio {
        builder.get_optimal_audio_stream(options)
    } else {
        builder.get_optimal_video_stream(options)
    }
}

/// Runs the DLNA [`StreamBuilder`] for one media source against the client's
/// profile and stamps the resulting play decision onto it: direct-play stays as-is,
/// otherwise `SupportsDirectPlay` is cleared and a `TranscodingUrl` (the HLS master
/// / remux stream, with the negotiated codecs) is set so the client transcodes.
/// Returns the decision summary for the metrics recorder ([`None`] when the
/// builder produced no stream).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // mirrors SetDeviceSpecificData
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
    policy: &ferrofin_model::users::UserPolicy,
    is_audio: bool,
    support: &FerrofinTranscoderSupport<'_>,
) -> Option<StreamDecision> {
    // MediaInfoHelper applies this again after negotiation, which may promote
    // a remux to Transcode. Audio-only sources require the audio permission;
    // video sources can still use HLS when any of the three operations is allowed.
    let policy_allows_transcoding = policy.enable_audio_playback_transcoding
        || (!is_audio
            && (policy.enable_video_playback_transcoding || policy.enable_playback_remuxing));
    if !policy_allows_transcoding || !flags.enable_transcoding {
        source.supports_transcoding = false;
        source.transcoding_url = None;
    }
    if !is_audio {
        source.supports_direct_stream &= policy.enable_playback_remuxing;
    }
    let mut options = MediaOptions::new(profile.clone());
    options.item_id = item_id;
    options.media_source_id.clone_from(&source.id);
    options.device_id = device_id.map(str::to_owned);
    options.max_bitrate = max_streaming_bitrate;
    // The client's capability veto (EnableDirectPlay/EnableDirectStream): the
    // builder never picks a vetoed method.
    options.enable_direct_play = flags.enable_direct_play;
    options.enable_direct_stream = flags.enable_direct_stream;
    // The client's explicit stream picks. A `-1` subtitle index ("off") passes
    // through as upstream: it matches no stream, whereas leaving it unset
    // would fall back to the default subtitle.
    options.audio_stream_index = stream_selection.audio_stream_index;
    options.subtitle_stream_index = stream_selection.subtitle_stream_index;
    options.max_audio_channels = stream_selection.max_audio_channels;
    options.always_burn_in_subtitle_when_transcoding =
        stream_selection.always_burn_in_subtitle_when_transcoding;
    options.media_sources = vec![source.clone()];

    let mut stream = optimal_stream(support, &options, is_audio)?;
    stream.start_position_ticks = stream_selection.start_time_ticks.unwrap_or(0);

    let profile_type = if is_audio {
        DlnaProfileType::Audio
    } else {
        DlnaProfileType::Video
    };
    // MediaInfoHelper recomputes capabilities after choosing a profile. In
    // particular, remux-only users can use an HLS profile even when the static
    // source's video-transcoding capability was false.
    source.supports_transcoding = flags.enable_transcoding
        && policy_allows_transcoding
        && (stream.play_method == PlayMethod::DirectStream
            || source.transcoding_container.is_some()
            || profile
                .transcoding_profiles
                .iter()
                .any(|p| p.profile_type == profile_type && p.context == options.context));
    let force_remote = source.is_remote && policy.force_remote_source_transcoding;
    if force_remote && stream.play_method == PlayMethod::DirectPlay {
        return Some(force_remote_delivery(
            source,
            stream,
            token,
            play_session_id,
            support,
        ));
    }
    if stream.play_method == PlayMethod::DirectPlay {
        source.supports_direct_play = true;
        source.supports_direct_stream = true;
        apply_subtitle_delivery(source, &stream, support, token);
        return Some(StreamDecision {
            play_method: "DirectPlay",
            transcode_reasons: String::new(),
            target_container: None,
            target_video_codec: None,
            target_audio_codec: None,
        });
    }
    if is_audio {
        return Some(audio_transcode_decision(
            source,
            stream,
            token,
            play_session_id,
            flags,
            force_remote,
            support,
        ));
    }
    // Not direct-play: deliver an HLS transcode. The negotiated codecs (typically
    // copy the compatible video, re-encode only the incompatible audio) are carried
    // on the StreamInfo; we pin the delivery to HLS because the builder can mislabel
    // an audio-only re-encode as a raw-copy DirectStream, which would hand the client
    // the untouched incompatible audio (the "video plays, no audio" case).
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
    set_hls_subtitle_delivery(&mut stream, source, profile, &container, support);

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
    if !source.supports_transcoding {
        source.supports_transcoding = false;
        apply_subtitle_delivery(source, &stream, support, token);
        return Some(StreamDecision::transcode(&stream));
    }
    // The playback-session id rides the transcode URL so the spawned job is
    // psid-addressable (`DELETE /Videos/ActiveEncodings?playSessionId=…`).
    stream.play_session_id = Some(play_session_id.to_owned());
    let mut transcoding_url = stream.to_url(None, token, None);
    // The C# controller appends the copy vetoes onto the negotiated URL; the
    // transcoder reads them back as `allowVideoStreamCopy`/`allowAudioStreamCopy`.
    if force_remote || !flags.allow_video_stream_copy {
        transcoding_url.push_str("&AllowVideoStreamCopy=false");
    }
    if force_remote || !flags.allow_audio_stream_copy {
        transcoding_url.push_str("&AllowAudioStreamCopy=false");
    }
    push_burn_in(&mut transcoding_url, &stream);
    source.transcoding_url = Some(transcoding_url);
    source.transcoding_sub_protocol = MediaStreamProtocol::hls;
    source.transcoding_container = Some(container.clone());
    apply_subtitle_delivery(source, &stream, support, token);
    Some(StreamDecision::transcode(&stream))
}

/// `MediaInfoHelper.SetDeviceSpecificData` for an audio item that is not
/// direct played. A `DirectStream` (container remux over HTTP, which the
/// server does not offer — `EnableDirectStream` is off) and a transcode both
/// become a transcode at the stream's own `/audio/{id}/stream.{container}` (or
/// `master.m3u8`) URL, with the profile's container and sub-protocol. There is
/// no HLS pin: the builder's audio decision is what the client asked for.
fn audio_transcode_decision(
    source: &mut MediaSourceInfo,
    mut stream: ferrofin_model::dlna::StreamInfo,
    token: Option<&str>,
    play_session_id: &str,
    flags: PlaybackFlags,
    force_remote: bool,
    support: &FerrofinTranscoderSupport<'_>,
) -> StreamDecision {
    source.supports_direct_play = false;
    source.supports_direct_stream = false;
    if source.supports_transcoding {
        stream.play_method = PlayMethod::Transcode;
        stream.play_session_id = Some(play_session_id.to_owned());
        let mut url = stream.to_url(None, token, None);
        if force_remote || !flags.allow_video_stream_copy {
            url.push_str("&AllowVideoStreamCopy=false");
        }
        if force_remote || !flags.allow_audio_stream_copy {
            url.push_str("&AllowAudioStreamCopy=false");
        }
        push_burn_in(&mut url, &stream);
        source.transcoding_url = Some(url);
        source.transcoding_container.clone_from(&stream.container);
        source.transcoding_sub_protocol = stream.sub_protocol;
    }
    apply_subtitle_delivery(source, &stream, support, token);
    StreamDecision::transcode(&stream)
}

/// MediaInfoHelper's remote-source branch preserves the negotiated protocol
/// and container even for an otherwise direct-playable source. In particular,
/// upstream can return an HTTP Static URL here; the flag is a negotiation rule,
/// not an unconditional encoder or stream-authorization gate.
fn force_remote_delivery(
    source: &mut MediaSourceInfo,
    mut stream: ferrofin_model::dlna::StreamInfo,
    token: Option<&str>,
    play_session_id: &str,
    support: &FerrofinTranscoderSupport<'_>,
) -> StreamDecision {
    source.supports_direct_play = false;
    source.supports_direct_stream = false;
    stream.play_session_id = Some(play_session_id.to_owned());
    let mut url = format!(
        "{}&AllowVideoStreamCopy=false&AllowAudioStreamCopy=false",
        stream.to_url(None, token, None)
    );
    push_burn_in(&mut url, &stream);
    source.transcoding_url = Some(url);
    source.transcoding_container.clone_from(&stream.container);
    source.transcoding_sub_protocol = stream.sub_protocol;
    apply_subtitle_delivery(source, &stream, support, token);
    StreamDecision::transcode(&stream)
}

/// `SetDeviceSpecificData` appends the burn-in request to every transcoding URL
/// it builds, so the transcoder burns in the subtitle the client chose.
fn push_burn_in(url: &mut String, stream: &ferrofin_model::dlna::StreamInfo) {
    if stream.always_burn_in_subtitle_when_transcoding {
        url.push_str("&alwaysBurnInSubtitleWhenTranscoding=true");
    }
}

/// Recomputes the selected subtitle's delivery after a decision is pinned to HLS.
fn set_hls_subtitle_delivery(
    stream: &mut ferrofin_model::dlna::StreamInfo,
    source: &MediaSourceInfo,
    profile: &DeviceProfile,
    container: &str,
    support: &FerrofinTranscoderSupport<'_>,
) {
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
            support,
            Some(container),
            Some(MediaStreamProtocol::hls),
        );
        stream.subtitle_delivery_method = subtitle_profile.method;
        stream.subtitle_format.clone_from(&subtitle_profile.format);
        stream.subtitle_codecs = subtitle_profile.format.clone().into_iter().collect();
    }
}

/// Populates each subtitle stream's `DeliveryMethod`/`DeliveryUrl` on the source
/// (Jellyfin's `StreamInfo.GetSubtitleProfiles`). Without it every subtitle has
/// no delivery method, so the client can't fetch (text → external VTT) or
/// request burn-in (image subs like DVDSUB/PGS → `Encode`), and a selected
/// subtitle simply never renders.
///
/// Also stamps the decision's tracks and reasons onto the source
/// (`SetDeviceSpecificSubtitleInfo` and the `DefaultAudioStreamIndex` line
/// after it): the client marks those tracks active when playback starts.
fn apply_subtitle_delivery(
    source: &mut MediaSourceInfo,
    stream: &ferrofin_model::dlna::StreamInfo,
    support: &FerrofinTranscoderSupport<'_>,
    token: Option<&str>,
) {
    source.default_audio_stream_index = stream.audio_stream_index;
    source.default_subtitle_stream_index = stream.subtitle_stream_index;
    source.transcode_reasons = stream.transcode_reasons;
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
    parts: Parts,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    // The GET form carries no device profile, so every source keeps its static
    // (direct-play) shape.
    let request = PlaybackRequest {
        user_id: query.user_id,
        flags: flags_from(&query, None),
        ..PlaybackRequest::default()
    };
    let client = super::system::context_from_parts(&state, &parts);
    Ok(Json(
        playback_info(&state, &auth, &client, item_id, request).await?,
    ))
}

/// The `POST /Items/{itemId}/PlaybackInfo` body — the client's device profile,
/// the streaming limits, and the stream-index selections used to negotiate the
/// play method. Query parameters take precedence over body fields (the C#
/// `?? playbackInfoDto?.Field` pattern).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
#[cfg_attr(test, derive(serde::Serialize))]
struct PlaybackInfoBody {
    #[serde(default, with = "ferrofin_model::json::guid::option")]
    user_id: Option<Uuid>,
    #[serde(default)]
    device_profile: Option<DeviceProfile>,
    // Track pickers post quoted indexes; the shared body binder reads them.
    // The marker additionally recognizes an exact empty nullable value.
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    max_streaming_bitrate: Option<i32>,
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    audio_stream_index: Option<i32>,
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    subtitle_stream_index: Option<i32>,
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    start_time_ticks: Option<i64>,
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    max_audio_channels: Option<i32>,
    #[serde(default)]
    media_source_id: Option<String>,
    #[serde(default)]
    live_stream_id: Option<String>,
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    auto_open_live_stream: Option<bool>,
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    always_burn_in_subtitle_when_transcoding: Option<bool>,
    #[serde(default)]
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    enable_direct_play: Option<bool>,
    #[serde(default)]
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    enable_direct_stream: Option<bool>,
    #[serde(default)]
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    enable_transcoding: Option<bool>,
    #[serde(default)]
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    allow_video_stream_copy: Option<bool>,
    #[serde(default)]
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
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
/// Port of `MediaInfoController.GetPostedPlaybackInfo`: the query and the
/// posted [`PlaybackInfoBody`] are folded together (query wins), the sources
/// are narrowed to the requested version or live stream, and each is
/// negotiated against the device profile (the posted one, else the device's
/// stored capabilities).
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
    parts: Parts,
    body: Option<JsonBody<PlaybackInfoBody>>,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    let mut body = body.map(|JsonBody(b)| b).unwrap_or_default();
    // A client that posts no profile negotiates with the one its device last
    // reported through `/Sessions/Capabilities`.
    let profile = match body.device_profile.take() {
        Some(profile) => Some(profile),
        None => {
            state
                .devices
                .get_capabilities(auth.device_id.as_deref())
                .await?
                .device_profile
        }
    };
    let request = PlaybackRequest {
        user_id: query.user_id.or(body.user_id),
        profile,
        max_streaming_bitrate: query.max_streaming_bitrate.or(body.max_streaming_bitrate),
        media_source_id: query
            .media_source_id
            .clone()
            .or(body.media_source_id.take()),
        live_stream_id: query.live_stream_id.clone().or(body.live_stream_id.take()),
        auto_open_live_stream: query
            .auto_open_live_stream
            .or(body.auto_open_live_stream)
            .unwrap_or(false),
        selection: StreamSelection {
            audio_stream_index: query.audio_stream_index.or(body.audio_stream_index),
            subtitle_stream_index: query.subtitle_stream_index.or(body.subtitle_stream_index),
            max_audio_channels: query.max_audio_channels.or(body.max_audio_channels),
            start_time_ticks: query.start_time_ticks.or(body.start_time_ticks),
            // Body only: the query form has no burn-in parameter.
            always_burn_in_subtitle_when_transcoding: body
                .always_burn_in_subtitle_when_transcoding
                .unwrap_or(false),
        },
        flags: flags_from(&query, Some(&body)),
    };
    let client = super::system::context_from_parts(&state, &parts);
    Ok(Json(
        playback_info(&state, &auth, &client, item_id, request).await?,
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

/// The posted `OpenLiveStreamDto` body.
///
/// Every scalar the query carries may instead arrive here — jellyfin-web and the
/// Android clients POST the whole request as a body — and the query wins where
/// both are present (C# `openToken ?? openLiveStreamDto?.OpenToken`). The device
/// profile it may also carry is not yet honoured.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
#[cfg_attr(test, derive(serde::Serialize))]
struct OpenLiveStreamDto {
    /// The open token identifying the source to open.
    open_token: Option<String>,
    /// The target user.
    #[serde(with = "ferrofin_model::json::guid::option")]
    user_id: Option<Uuid>,
    /// The play session id.
    play_session_id: Option<String>,
    /// The maximum streaming bitrate.
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    max_streaming_bitrate: Option<i32>,
    /// The start time in ticks.
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    start_time_ticks: Option<i64>,
    /// The audio stream index.
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    audio_stream_index: Option<i32>,
    /// The subtitle stream index.
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    subtitle_stream_index: Option<i32>,
    /// The maximum number of audio channels.
    #[serde(deserialize_with = "ferrofin_model::json::value::nullable")]
    max_audio_channels: Option<i32>,
    /// The item id whose source is opened.
    #[serde(with = "ferrofin_model::json::guid::option")]
    item_id: Option<Uuid>,
    /// Whether direct play is enabled.
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    enable_direct_play: Option<bool>,
    /// Whether direct stream is enabled.
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
    enable_direct_stream: Option<bool>,
    /// Whether subtitles are always burned in when transcoding.
    #[serde(default, deserialize_with = "ferrofin_model::json::value::nullable")]
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
    parts: Parts,
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
    let mut media_source = state.media_sources.open_live_stream(&request).await?;
    let client = super::system::context_from_parts(&state, &parts);
    publish_live_stream_path(&state, &client, &mut media_source).await;
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

crate::query::query_parameters! {
    PlaybackInfoQuery {} => [("get", "/Items/{itemId}/PlaybackInfo"), ("post", "/Items/{itemId}/PlaybackInfo")];
    OpenLiveStreamQuery {} => [("post", "/LiveStreams/Open")];
    CloseLiveStreamQuery {} => [("post", "/LiveStreams/Close")];
    BitrateTestQuery {} => [("get", "/Playback/BitrateTest")];
}

#[cfg(test)]
mod published_live_stream_path_tests {
    use super::published_live_stream_path;
    use ferrofin_model::media_info::MediaProtocol;

    /// `MediaInfoHelperTests.LiveStreamFilesPath`.
    const LIVE: &str = "/LiveTv/LiveStreamFiles/abc/stream.ts";

    /// `MediaInfoHelperTests.GetPublishedLiveStreamPath_VariousInputs_ReturnsExpected`
    /// (v12.2), row for row.
    #[rstest::rstest]
    #[case("https://media.example.com", &format!("http://172.19.0.3:8096{LIVE}"), MediaProtocol::Http, "", Some(format!("https://media.example.com{LIVE}")))]
    #[case("https://media.example.com/", &format!("http://172.19.0.3:8096{LIVE}?token=1"), MediaProtocol::Http, "", Some(format!("https://media.example.com{LIVE}?token=1")))]
    #[case("https://media.example.com", &format!("http://172.19.0.3:8096{LIVE}#fragment"), MediaProtocol::Http, "", Some(format!("https://media.example.com{LIVE}")))]
    #[case("https://media.example.com", &format!("https://172.19.0.3:8920{LIVE}"), MediaProtocol::Http, "", Some(format!("https://media.example.com{LIVE}")))]
    #[case("https://media.example.com", &format!("http://192.168.1.10:8096{LIVE}"), MediaProtocol::Http, "", Some(format!("https://media.example.com{LIVE}")))]
    #[case("https://media.example.com:8920", &format!("http://172.19.0.3:8096{LIVE}"), MediaProtocol::Http, "", Some(format!("https://media.example.com:8920{LIVE}")))]
    #[case("https://media.example.com", &format!("http://172.19.0.3:8096/jellyfin{LIVE}"), MediaProtocol::Http, "/jellyfin", Some(format!("https://media.example.com/jellyfin{LIVE}")))]
    #[case("https://jellyfin", &format!("http://172.19.0.3:8096/jellyfin{LIVE}"), MediaProtocol::Http, "/jellyfin", Some(format!("https://jellyfin/jellyfin{LIVE}")))]
    #[case("https://media.example.com/jellyfin", &format!("http://172.19.0.3:8096/jellyfin{LIVE}"), MediaProtocol::Http, "/jellyfin", Some(format!("https://media.example.com/jellyfin{LIVE}")))]
    #[case("https://media.example.com/jellyfin/", &format!("http://172.19.0.3:8096/jellyfin{LIVE}"), MediaProtocol::Http, "/jellyfin", Some(format!("https://media.example.com/jellyfin{LIVE}")))]
    #[case("https://media.example.com", &format!("http://172.19.0.3:8096/jellyfin2{LIVE}"), MediaProtocol::Http, "/jellyfin", None)]
    #[case(
        "https://media.example.com",
        "/media/livetv/buffer/abc/stream.ts",
        MediaProtocol::File,
        "",
        None
    )]
    #[case(
        "https://media.example.com",
        "not a uri",
        MediaProtocol::Http,
        "",
        None
    )]
    fn get_published_live_stream_path_various_inputs_returns_expected(
        #[case] smart_api_url: &str,
        #[case] local_path: &str,
        #[case] protocol: MediaProtocol,
        #[case] base_url: &str,
        #[case] expected: Option<String>,
    ) {
        assert_eq!(
            published_live_stream_path(smart_api_url, Some(local_path), protocol, base_url),
            expected
        );
    }

    /// `OpenMediaSource_NotAPublishableLiveStreamFilesPath_PathUnchanged`: a
    /// tuner's own URL, a file, another server route and a library `.strm`
    /// are never rewritten.
    #[rstest::rstest]
    #[case(MediaProtocol::Http, "http://192.168.1.50:5004/live/channel1.ts")]
    #[case(MediaProtocol::File, "/media/livetv/buffer/abc/stream.ts")]
    #[case(MediaProtocol::Http, "http://172.19.0.3:8096/Videos/abc/stream.ts")]
    #[case(MediaProtocol::Http, "http://172.19.0.3:8096/library/movie.strm")]
    fn a_path_outside_the_live_stream_buffer_is_left_alone(
        #[case] protocol: MediaProtocol,
        #[case] path: &str,
    ) {
        assert_eq!(
            published_live_stream_path("https://media.example.com", Some(path), protocol, ""),
            None
        );
    }
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

    /// `MediaInfoHelperTests.CreateSource`.
    fn sort_source(
        item_id: uuid::Uuid,
        bitrate: i32,
        supports_direct_play: bool,
    ) -> MediaSourceInfo {
        MediaSourceInfo {
            id: Some(item_id.simple().to_string()),
            bitrate: Some(bitrate),
            supports_direct_play,
            supports_direct_stream: true,
            supports_transcoding: true,
            ..MediaSourceInfo::default()
        }
    }

    #[test]
    fn sort_media_sources_preferred_item_exceeds_bitrate_stays_default() {
        let preferred_item_id = uuid::Uuid::new_v4();
        let preferred = sort_source(preferred_item_id, 80_000_000, false);
        let sibling = sort_source(uuid::Uuid::new_v4(), 8_000_000, true);
        let mut sources = vec![sibling, preferred.clone()];
        super::sort_media_sources(&mut sources, Some(20_000_000), preferred_item_id);
        assert_eq!(sources[0].id, preferred.id);
    }

    #[test]
    fn sort_media_sources_no_preferred_item_orders_by_playability() {
        let direct_play = sort_source(uuid::Uuid::new_v4(), 8_000_000, true);
        let mut transcode_only = sort_source(uuid::Uuid::new_v4(), 8_000_000, false);
        transcode_only.supports_direct_stream = false;
        let mut sources = vec![transcode_only, direct_play.clone()];
        super::sort_media_sources(&mut sources, Some(20_000_000), uuid::Uuid::nil());
        assert_eq!(sources[0].id, direct_play.id);
    }

    #[test]
    fn sort_media_sources_preferred_id_not_in_sources_keeps_playability_order() {
        let direct_play = sort_source(uuid::Uuid::new_v4(), 8_000_000, true);
        let mut transcode_only = sort_source(uuid::Uuid::new_v4(), 8_000_000, false);
        transcode_only.supports_direct_stream = false;
        let mut sources = vec![transcode_only, direct_play.clone()];
        super::sort_media_sources(&mut sources, Some(20_000_000), uuid::Uuid::new_v4());
        assert_eq!(sources[0].id, direct_play.id);
    }

    #[test]
    fn sort_media_sources_puts_profiles_that_fit_the_bitrate_first() {
        // Multi-profile Live TV channels (HDHomeRun EXTEND) arrive widest
        // first; under a 4 Mbps limit the fitting profiles lead, unknown
        // bitrates follow, and the order is otherwise kept.
        let channel = |id: &str, bitrate: Option<i32>| MediaSourceInfo {
            id: Some(id.to_owned()),
            protocol: ferrofin_model::media_info::MediaProtocol::Http,
            bitrate,
            supports_direct_stream: true,
            ..MediaSourceInfo::default()
        };
        let mut sources = vec![
            channel("heavy", Some(15_000_000)),
            channel("internet540", Some(2_500_000)),
            channel("unknown", None),
            channel("internet360", Some(1_000_000)),
        ];
        super::sort_media_sources(&mut sources, Some(4_000_000), uuid::Uuid::nil());
        let order: Vec<_> = sources.iter().filter_map(|s| s.id.as_deref()).collect();
        assert_eq!(order, ["internet540", "internet360", "unknown", "heavy"]);
    }

    #[test]
    fn open_live_stream_reads_the_posted_body_and_lets_the_query_win() {
        // The shape jellyfin-web (and the parity harness) POSTs: everything in
        // the body, nothing in the query. Before this was bound, the open token
        // never reached the manager and a Live TV channel could not be opened.
        let dto: super::OpenLiveStreamDto = crate::extract::deserialize_mvc(
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
        let body: super::PlaybackInfoBody = crate::extract::deserialize_mvc(
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
    fn transcode_carries_the_start_position_burn_in_and_channel_limit() {
        use ferrofin_model::dlna::DirectPlayProfile;
        // AC3 6ch is outside the profile, so the source is transcoded.
        let profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                container: "mp4".to_owned(),
                video_codec: Some("h264".to_owned()),
                audio_codec: Some("aac".to_owned()),
                profile_type: DlnaProfileType::Video,
            }],
            transcoding_profiles: vec![TranscodingProfile {
                container: "ts".to_owned(),
                video_codec: "h264".to_owned(),
                audio_codec: "aac".to_owned(),
                profile_type: DlnaProfileType::Video,
                protocol: MediaStreamProtocol::hls,
                ..TranscodingProfile::default()
            }],
            ..DeviceProfile::default()
        };
        let policy = ferrofin_model::users::UserPolicy {
            enable_audio_playback_transcoding: true,
            enable_video_playback_transcoding: true,
            enable_playback_remuxing: true,
            ..ferrofin_model::users::UserPolicy::default()
        };
        let transcode = |selection: super::StreamSelection| {
            let mut source = source_with_video("h264");
            source.container = Some("mp4".to_owned());
            source.supports_transcoding = true;
            source.media_streams.push(MediaStream {
                stream_type: MediaStreamType::Audio,
                codec: Some("ac3".to_owned()),
                channels: Some(6),
                index: 1,
                ..MediaStream::default()
            });
            super::apply_stream_decision(
                &mut source,
                &profile,
                uuid::Uuid::from_u128(1),
                None,
                None,
                None,
                selection,
                "session",
                super::PlaybackFlags::default(),
                &policy,
                false,
                &support(),
            );
            // SetDeviceSpecificSubtitleInfo stamps the decision's reasons.
            assert!(
                source
                    .transcode_reasons
                    .contains(ferrofin_model::session::TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED),
                "{:?}",
                source.transcode_reasons
            );
            source.transcoding_url.expect("transcode")
        };
        let plain = transcode(super::StreamSelection::default());
        assert!(!plain.contains("StartTimeTicks"), "{plain}");
        assert!(!plain.contains("alwaysBurnIn"), "{plain}");
        let url = transcode(super::StreamSelection {
            max_audio_channels: Some(2),
            start_time_ticks: Some(600_000_000),
            always_burn_in_subtitle_when_transcoding: true,
            ..super::StreamSelection::default()
        });
        assert!(url.contains("StartTimeTicks=600000000"), "{url}");
        assert!(
            url.contains("&alwaysBurnInSubtitleWhenTranscoding=true"),
            "{url}"
        );
        // The channel cap is the builder's global limit: a stereo target
        // gets a smaller audio bitrate than the 6ch source would.
        let audio_bitrate = |url: &str| {
            url.split(['?', '&'])
                .find_map(|kv| kv.strip_prefix("AudioBitrate="))
                .map(str::to_owned)
        };
        assert_ne!(audio_bitrate(&url), audio_bitrate(&plain), "{url}");
    }

    #[test]
    fn remote_bitrate_limit_matches_user_override_and_lan_rules() {
        for (client, user, server, local, expected) in [
            (Some(8_000_000), 0, 1_000_000, false, Some(1_000_000)),
            (
                Some(8_000_000),
                2_000_000,
                1_000_000,
                false,
                Some(2_000_000),
            ),
            (Some(500_000), 2_000_000, 1_000_000, false, Some(500_000)),
            (None, 2_000_000, 1_000_000, false, Some(2_000_000)),
            (None, -1, 1_000_000, false, Some(1_000_000)),
            (None, 0, 0, false, None),
            (Some(8_000_000), 0, -1, false, Some(8_000_000)),
            (Some(8_000_000), 1_000_000, 500_000, true, Some(8_000_000)),
            (None, 1_000_000, 500_000, true, None),
            // Upstream preserves explicit client zero/negative values.
            (Some(0), 1_000_000, 0, false, Some(0)),
            (Some(-1), 1_000_000, 0, false, Some(-1)),
        ] {
            assert_eq!(
                super::remote_bitrate_limit(client, user, server, local),
                expected
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one source/profile fixture and policy matrix
    fn remote_source_forcing_is_live_scoped_and_preserves_policy_vetoes() {
        use ferrofin_model::dlna::DirectPlayProfile;
        let profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                container: "mp4".to_owned(),
                video_codec: Some("h264".to_owned()),
                audio_codec: Some("aac".to_owned()),
                profile_type: DlnaProfileType::Video,
            }],
            transcoding_profiles: vec![TranscodingProfile {
                container: "ts".to_owned(),
                video_codec: "h264".to_owned(),
                audio_codec: "aac".to_owned(),
                profile_type: DlnaProfileType::Video,
                protocol: MediaStreamProtocol::hls,
                ..TranscodingProfile::default()
            }],
            ..DeviceProfile::default()
        };
        for is_remote in [false, true] {
            for force in [false, true, false] {
                for direct in [false, true] {
                    let mut source = MediaSourceInfo {
                        is_remote,
                        container: Some("mp4".to_owned()),
                        bitrate: Some(1_000_000),
                        media_streams: vec![
                            MediaStream {
                                stream_type: MediaStreamType::Video,
                                codec: Some("h264".to_owned()),
                                ..MediaStream::default()
                            },
                            MediaStream {
                                stream_type: MediaStreamType::Audio,
                                codec: Some("aac".to_owned()),
                                index: 1,
                                ..MediaStream::default()
                            },
                        ],
                        ..MediaSourceInfo::default()
                    };
                    let policy = ferrofin_model::users::UserPolicy {
                        force_remote_source_transcoding: force,
                        enable_audio_playback_transcoding: true,
                        enable_video_playback_transcoding: true,
                        enable_playback_remuxing: true,
                        ..ferrofin_model::users::UserPolicy::default()
                    };
                    let flags = super::PlaybackFlags {
                        enable_direct_play: direct,
                        ..super::PlaybackFlags::default()
                    };
                    super::apply_stream_decision(
                        &mut source,
                        &profile,
                        uuid::Uuid::from_u128(1),
                        None,
                        None,
                        None,
                        super::StreamSelection::default(),
                        "session",
                        flags,
                        &policy,
                        false,
                        &support(),
                    );
                    assert_eq!(source.supports_direct_play, direct && !(is_remote && force));
                    let url = source.transcoding_url.as_deref().unwrap_or_default();
                    assert_eq!(
                        url.contains("AllowVideoStreamCopy=false"),
                        is_remote && force
                    );
                    assert_eq!(
                        url.contains("AllowAudioStreamCopy=false"),
                        is_remote && force
                    );
                    if direct && is_remote && force {
                        assert!(
                            url.contains("/stream.mp4?"),
                            "upstream preserves direct HTTP delivery: {url}"
                        );
                        assert!(url.contains("Static=true"));
                    }
                    let mut denied = policy;
                    denied.enable_video_playback_transcoding = false;
                    denied.enable_audio_playback_transcoding = false;
                    denied.enable_playback_remuxing = false;
                    super::apply_stream_decision(
                        &mut source,
                        &profile,
                        uuid::Uuid::from_u128(1),
                        None,
                        None,
                        None,
                        super::StreamSelection::default(),
                        "session",
                        flags,
                        &denied,
                        false,
                        &support(),
                    );
                    assert!(
                        !source.supports_transcoding,
                        "forcing never grants transcoding permission"
                    );
                }
            }
        }
    }

    #[test]
    fn negotiated_capabilities_respect_every_playback_permission_combination() {
        use ferrofin_model::dlna::TranscodingProfile;
        use ferrofin_model::users::UserPolicy;
        for is_video in [false, true] {
            for bits in 0..8 {
                let policy = UserPolicy {
                    enable_video_playback_transcoding: bits & 1 != 0,
                    enable_audio_playback_transcoding: bits & 2 != 0,
                    enable_playback_remuxing: bits & 4 != 0,
                    ..UserPolicy::default()
                };
                let mut streams = vec![MediaStream {
                    stream_type: MediaStreamType::Audio,
                    codec: Some("aac".to_owned()),
                    ..MediaStream::default()
                }];
                if is_video {
                    streams.push(MediaStream {
                        stream_type: MediaStreamType::Video,
                        codec: Some("h264".to_owned()),
                        index: 1,
                        ..MediaStream::default()
                    });
                }
                let profile = DeviceProfile {
                    transcoding_profiles: vec![TranscodingProfile {
                        container: "ts".to_owned(),
                        video_codec: "h264".to_owned(),
                        audio_codec: "aac".to_owned(),
                        profile_type: if is_video {
                            DlnaProfileType::Video
                        } else {
                            DlnaProfileType::Audio
                        },
                        protocol: MediaStreamProtocol::hls,
                        ..TranscodingProfile::default()
                    }],
                    ..DeviceProfile::default()
                };
                for client_allows in [false, true] {
                    let mut source = MediaSourceInfo {
                        media_streams: streams.clone(),
                        container: Some("mkv".to_owned()),
                        ..MediaSourceInfo::default()
                    };
                    let flags = super::PlaybackFlags {
                        enable_direct_play: false,
                        enable_transcoding: client_allows,
                        ..super::PlaybackFlags::default()
                    };
                    super::apply_stream_decision(
                        &mut source,
                        &profile,
                        uuid::Uuid::from_u128(1),
                        None,
                        None,
                        None,
                        super::StreamSelection::default(),
                        "session",
                        flags,
                        &policy,
                        !is_video,
                        &support(),
                    );
                    let expected = client_allows
                        && (policy.enable_audio_playback_transcoding
                            || (is_video
                                && (policy.enable_video_playback_transcoding
                                    || policy.enable_playback_remuxing)));
                    assert_eq!(
                        source.supports_transcoding, expected,
                        "video={is_video} bits={bits}"
                    );
                    assert_eq!(source.transcoding_url.is_some(), expected);
                    // An audio source that cannot transcode gets no decision at
                    // all (`GetOptimalAudioStream` returns null) and keeps the
                    // static source's flags, as upstream; every other case
                    // decides, and none of these direct plays.
                    assert_eq!(
                        source.supports_direct_play,
                        !is_video && !expected,
                        "video={is_video} bits={bits}"
                    );
                }
            }
        }
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
                subtitle_stream_index: Some(2),
                ..super::StreamSelection::default()
            },
            "ps1",
            flags,
            &ferrofin_model::users::UserPolicy {
                enable_audio_playback_transcoding: true,
                enable_video_playback_transcoding: true,
                enable_playback_remuxing: true,
                ..ferrofin_model::users::UserPolicy::default()
            },
            false,
            &support(),
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

    /// `MediaInfoHelper.SetDeviceSpecificData` for a music item: a direct play
    /// stays one, an unplayable file becomes an audio transcode at
    /// `/audio/{id}/stream.{container}` (never an HLS video master), a veto of
    /// transcoding leaves it with no URL, and a disallowed audio transcode
    /// does too.
    #[test]
    #[allow(clippy::too_many_lines)] // one profile fixture and its scenarios
    fn music_is_negotiated_by_the_audio_builder_not_the_video_one() {
        use ferrofin_model::dlna::{DirectPlayProfile, TranscodingProfile};
        let profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                container: "mp3".to_owned(),
                audio_codec: Some("mp3".to_owned()),
                video_codec: None,
                profile_type: DlnaProfileType::Audio,
            }],
            transcoding_profiles: vec![TranscodingProfile {
                container: "mp3".to_owned(),
                audio_codec: "mp3".to_owned(),
                profile_type: DlnaProfileType::Audio,
                protocol: MediaStreamProtocol::http,
                context: ferrofin_model::dlna::EncodingContext::Streaming,
                ..TranscodingProfile::default()
            }],
            ..DeviceProfile::default()
        };
        let track = |container: &str, codec: &str, cover_art: bool| {
            let mut streams = vec![MediaStream {
                stream_type: MediaStreamType::Audio,
                codec: Some(codec.to_owned()),
                channels: Some(2),
                index: 0,
                ..MediaStream::default()
            }];
            if cover_art {
                // An embedded cover is a video stream of an audio file.
                streams.push(MediaStream {
                    stream_type: MediaStreamType::Video,
                    codec: Some("mjpeg".to_owned()),
                    index: 1,
                    ..MediaStream::default()
                });
            }
            MediaSourceInfo {
                id: Some("0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a".to_owned()),
                container: Some(container.to_owned()),
                bitrate: Some(256_000),
                media_streams: streams,
                ..MediaSourceInfo::default()
            }
        };
        let policy = |audio_transcoding: bool| ferrofin_model::users::UserPolicy {
            enable_audio_playback_transcoding: audio_transcoding,
            ..ferrofin_model::users::UserPolicy::default()
        };
        let decide = |source: &mut MediaSourceInfo, policy, flags| {
            super::apply_stream_decision(
                source,
                &profile,
                uuid::Uuid::from_u128(0xA0D10),
                None,
                None,
                Some("device"),
                super::StreamSelection::default(),
                "ps-audio",
                flags,
                &policy,
                true,
                &support(),
            )
        };

        // Direct play, cover art and all.
        let mut mp3 = track("mp3", "mp3", true);
        let decision = decide(&mut mp3, policy(true), super::PlaybackFlags::default()).unwrap();
        assert_eq!(decision.play_method, "DirectPlay");
        assert!(mp3.supports_direct_play && mp3.transcoding_url.is_none());

        // A FLAC file this device cannot play is an audio transcode.
        let mut flac = track("flac", "flac", true);
        let decision = decide(&mut flac, policy(true), super::PlaybackFlags::default()).unwrap();
        assert_eq!(decision.play_method, "Transcode");
        assert!(!flac.supports_direct_play && !flac.supports_direct_stream);
        let url = flac.transcoding_url.as_deref().expect("a transcode URL");
        assert!(
            url.starts_with("/audio/00000000-0000-0000-0000-0000000a0d10/stream.mp3?"),
            "{url}"
        );
        assert!(
            !url.contains("master.m3u8") && url.contains("AudioCodec=mp3"),
            "{url}"
        );
        assert_eq!(flac.transcoding_container.as_deref(), Some("mp3"));
        assert_eq!(flac.transcoding_sub_protocol, MediaStreamProtocol::http);
        assert!(flac.supports_transcoding);

        // Copy vetoes ride the URL like the video ones.
        let mut vetoed = track("flac", "flac", false);
        let flags = super::PlaybackFlags {
            allow_audio_stream_copy: false,
            ..super::PlaybackFlags::default()
        };
        decide(&mut vetoed, policy(true), flags).unwrap();
        assert!(
            vetoed
                .transcoding_url
                .as_deref()
                .unwrap()
                .ends_with("&AllowAudioStreamCopy=false")
        );

        // No audio-transcoding permission, or a client veto: the source cannot
        // transcode, so the builder offers nothing (`GetOptimalAudioStream`
        // returns null) and no decision is made — no URL, as upstream.
        for (policy, flags) in [
            (policy(false), super::PlaybackFlags::default()),
            (
                policy(true),
                super::PlaybackFlags {
                    enable_transcoding: false,
                    ..super::PlaybackFlags::default()
                },
            ),
        ] {
            let mut denied = track("flac", "flac", false);
            assert!(decide(&mut denied, policy, flags).is_none());
            assert!(!denied.supports_transcoding);
            assert!(denied.transcoding_url.is_none());
        }
    }

    /// `MediaEncoder` as `ITranscoderSupport`: an audio codec is encodable
    /// when the probed build has its encoder, opus and mp3 asked as
    /// `libopus` and `libmp3lame`; any subtitle codec is; and extraction
    /// follows `EnableSubtitleExtraction`, whatever the codec.
    #[test]
    fn transcoder_support_asks_the_probed_encoders_and_the_extraction_setting() {
        let s = support();
        assert!(s.can_encode_to_audio_codec("AAC"), "case-insensitive");
        assert!(s.can_encode_to_audio_codec("opus"));
        assert!(s.can_encode_to_audio_codec("mp3"));
        assert!(!s.can_encode_to_audio_codec("truehd"), "not in the build");
        assert!(s.can_extract_subtitles("hdmv_pgs_subtitle"));
        let build = Encoders(&["aac", "opus", "mp3"]);
        let bare = FerrofinTranscoderSupport {
            encoders: &build,
            enable_subtitle_extraction: false,
        };
        assert!(
            !bare.can_encode_to_audio_codec("opus"),
            "opus needs libopus"
        );
        assert!(
            !bare.can_encode_to_audio_codec("mp3"),
            "mp3 needs libmp3lame"
        );
        assert!(bare.can_encode_to_subtitle_codec("anything"));
        assert!(!bare.can_extract_subtitles("subrip"));
    }

    /// An ffmpeg build's encoder list, compared ignoring case.
    struct Encoders(&'static [&'static str]);

    impl ferrofin_traits::media_encoding::EncoderSupport for Encoders {
        fn supports_encoder(&self, encoder: &str) -> bool {
            self.0.iter().any(|e| e.eq_ignore_ascii_case(encoder))
        }
    }

    /// The negotiation's capabilities with a common ffmpeg build's encoders
    /// and extraction on (the setting's default).
    fn support() -> FerrofinTranscoderSupport<'static> {
        FerrofinTranscoderSupport {
            encoders: &crate::test_support::CommonEncoders,
            enable_subtitle_extraction: true,
        }
    }
}

#[cfg(test)]
mod numeric_contract_tests {
    #[test]
    fn all_contract_numbers_accept_quoted_values() {
        assert_eq!(
            crate::extract::contract_numbers::check_model(
                "OpenLiveStreamDto",
                super::OpenLiveStreamDto::default()
            ),
            5
        );
        assert_eq!(
            crate::extract::contract_numbers::check_model(
                "PlaybackInfoDto",
                super::PlaybackInfoBody::default()
            ),
            5
        );
    }
}
