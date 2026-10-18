//! Audio streaming routes, direct play and progressive transcoding.
//! Ordinary stream requests honour Static (default false), requested codecs,
//! output container and HEAD. The universal route negotiates what to serve
//! from the client's declared containers, codecs and caps.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ferrofin_model::data::MediaStreamProtocol;
use ferrofin_model::dlna::{
    CodecProfile, CodecType, DeviceProfile, DirectPlayProfile, DlnaProfileType, EncodingContext,
    ProfileCondition, ProfileConditionType, ProfileConditionValue, TranscodingProfile,
};
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_model::session::TranscodeReasons;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
use crate::handlers::hls::nullable_enum_value;
use crate::handlers::media_info::universal_audio_source;
use crate::handlers::streaming::{serve_static_source, static_or_transcode, static_source};
use crate::state::AppState;

/// `GET`/`HEAD /Audio/{itemId}/stream` — stream the requested audio source.
///
/// Port of `AudioController.GetAudioStream`: `Static=true` serves the version
/// or open live stream the request names ([`static_or_transcode`]); anything
/// else is transcoded. Range requests yield `206 Partial Content`; `HEAD`
/// returns headers only.
async fn get_audio_stream(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<ferrofin_traits::options::AuthorizationInfo>,
    Path(item_id): Path<Uuid>,
    Query(query): Query<crate::handlers::hls::HlsQueryPub>,
    request: Request,
) -> Result<Response, ApiError> {
    let raw = request.uri().query().map(ToOwned::to_owned);
    let req = crate::handlers::hls::request_from_query(item_id, query, raw, &auth);
    crate::handlers::hls::validate_stream_request(&req)?;
    static_or_transcode(&state, item_id, true, req, request).await
}

/// The extension route has the same Static=false default as the bare route.
async fn get_audio_stream_by_container(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<ferrofin_traits::options::AuthorizationInfo>,
    Path((item_id, container)): Path<(Uuid, String)>,
    Query(query): Query<crate::handlers::hls::HlsQueryPub>,
    request: Request,
) -> Result<Response, ApiError> {
    let raw = request.uri().query().map(ToOwned::to_owned);
    let mut req = crate::handlers::hls::request_from_query(item_id, query, raw, &auth);
    req.output_container = Some(
        container
            .strip_prefix("stream.")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::NotFound("stream route".to_owned()))?
            .to_owned(),
    );
    crate::handlers::hls::validate_stream_request(&req)?;
    static_or_transcode(&state, item_id, true, req, request).await
}

/// The query parameters `GET /Audio/{itemId}/universal` accepts
/// (`UniversalAudioController.GetUniversalAudioStream`).
///
/// TODO(gl-277): bind `enableAudioVbrEncoding` (default true) once the
/// transcoder carries it; VBR audio encoding is not ported on any stream route.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UniversalAudioQuery {
    /// The containers the client plays as they are, each `container|codec|…`.
    #[serde(default)]
    container: Option<String>,
    /// The version to play; every version competes when absent.
    #[serde(default)]
    media_source_id: Option<String>,
    /// The requesting device (scopes the transcode job).
    #[serde(default)]
    device_id: Option<String>,
    /// Optional target user; defaults to the authenticated caller. Upstream
    /// resolves it through `RequestHelpers.GetUserId`, so naming another user as
    /// a non-administrator is a `403`.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// The codec to transcode to (mp3 when absent).
    #[serde(default)]
    audio_codec: Option<String>,
    /// The client's channel cap.
    #[serde(default)]
    max_audio_channels: Option<i32>,
    /// The channel count of the transcoding profile.
    #[serde(default)]
    transcoding_audio_channels: Option<i32>,
    /// The client's bitrate cap in bit/s.
    #[serde(default)]
    max_streaming_bitrate: Option<i32>,
    /// The bitrate to encode to, in bit/s.
    #[serde(default)]
    audio_bit_rate: Option<i32>,
    /// The start offset in ticks.
    #[serde(default)]
    start_time_ticks: Option<i64>,
    /// The container to transcode to (mp3 when absent).
    #[serde(default)]
    transcoding_container: Option<String>,
    /// The transcoding protocol (`http` when absent).
    #[serde(default, deserialize_with = "optional_stream_protocol")]
    transcoding_protocol: Option<MediaStreamProtocol>,
    /// The client's sample-rate cap.
    #[serde(default)]
    max_audio_sample_rate: Option<i32>,
    /// The client's bit-depth cap.
    #[serde(default)]
    max_audio_bit_depth: Option<i32>,
    /// Whether a remote source may be redirected to.
    #[serde(default)]
    enable_remote_media: Option<bool>,
    /// Whether redirection is allowed at all (default true).
    #[serde(default)]
    enable_redirection: Option<bool>,
}

/// Binds a nullable `MediaStreamProtocol` the way upstream's registered
/// `NullableEnumModelBinder` does ([`nullable_enum_value`]): a value that is no
/// member leaves it unset rather than failing the request.
fn optional_stream_protocol<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<MediaStreamProtocol>, D::Error> {
    use serde::Deserialize as _;
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value
        .and_then(|value| nullable_enum_value(&value, &[("http", 0), ("hls", 1)]))
        .map(|number| match number {
            0 => MediaStreamProtocol::http,
            1 => MediaStreamProtocol::hls,
            number => MediaStreamProtocol::Unrecognized(number),
        }))
}

/// `UniversalAudioController.GetDeviceProfile`: a direct-play profile per
/// listed container (`container|codec|…`), one streaming transcoding profile
/// (mp3 over HTTP unless the client names another), and a codec profile of
/// the client's sample-rate, bit-depth and channel caps.
fn universal_device_profile(query: &UniversalAudioQuery, containers: &[&str]) -> DeviceProfile {
    let direct_play_profiles = containers
        .iter()
        .filter_map(|entry| {
            let mut parts = entry.split('|').filter(|part| !part.is_empty());
            let container = parts.next()?.to_owned();
            let codecs: Vec<&str> = parts.collect();
            Some(DirectPlayProfile {
                container,
                audio_codec: (!codecs.is_empty()).then(|| codecs.join(",")),
                video_codec: None,
                profile_type: DlnaProfileType::Audio,
            })
        })
        .collect();
    let transcoding_profiles = vec![TranscodingProfile {
        profile_type: DlnaProfileType::Audio,
        context: EncodingContext::Streaming,
        container: query
            .transcoding_container
            .clone()
            .unwrap_or_else(|| "mp3".to_owned()),
        audio_codec: query
            .audio_codec
            .clone()
            .unwrap_or_else(|| "mp3".to_owned()),
        protocol: query
            .transcoding_protocol
            .unwrap_or(MediaStreamProtocol::http),
        max_audio_channels: query.transcoding_audio_channels.map(|c| c.to_string()),
        ..TranscodingProfile::default()
    }];
    let conditions: Vec<ProfileCondition> = [
        (
            ProfileConditionValue::AudioSampleRate,
            query.max_audio_sample_rate,
        ),
        (
            ProfileConditionValue::AudioBitDepth,
            query.max_audio_bit_depth,
        ),
        (
            ProfileConditionValue::AudioChannels,
            query.max_audio_channels,
        ),
    ]
    .into_iter()
    .filter_map(|(property, cap)| {
        Some(ProfileCondition {
            condition: ProfileConditionType::LessThanEqual,
            property,
            value: cap?.to_string(),
            is_required: false,
        })
    })
    .collect();
    let codec_profiles = if conditions.is_empty() {
        Vec::new()
    } else {
        vec![CodecProfile {
            codec_type: CodecType::Audio,
            container: Some(containers.join(",")),
            conditions,
            ..CodecProfile::default()
        }]
    };
    DeviceProfile {
        direct_play_profiles,
        transcoding_profiles,
        codec_profiles,
        ..DeviceProfile::default()
    }
}

/// The HLS segment containers ffmpeg writes (`mpegts` → `ts`, `fmp4` → `mp4`).
const HLS_SEGMENT_CONTAINERS: [&str; 2] = ["ts", "mp4"];

/// `GET`/`HEAD /Audio/{itemId}/universal` — stream the audio the client can play.
///
/// Port of `UniversalAudioController.GetUniversalAudioStream`: the client's
/// containers, codecs and caps become a device profile, the item's versions
/// are negotiated against it ([`universal_audio_source`]), and the best one is
/// served as the negotiation decided: a redirect to a remote source the client
/// may fetch itself, the original file (Range/`HEAD`), an HLS master playlist
/// (`transcodingProtocol=hls`), or a progressive transcode. Requires
/// authentication (`[Authorize]`).
async fn get_universal_audio_stream(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UniversalAudioQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    // Upstream's `[RegularExpression(ContainerValidationRegexStr)]` binding.
    for value in [&query.audio_codec, &query.transcoding_container]
        .into_iter()
        .flatten()
    {
        if !ferrofin_model::media_info::is_valid_stream_identifier(value) {
            return Err(ApiError::BadRequest(
                "invalid streaming container or codec".to_owned(),
            ));
        }
    }
    let containers: Vec<&str> = query
        .container
        .as_deref()
        .map(|c| {
            c.split(',')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let profile = universal_device_profile(&query, &containers);
    let negotiated = universal_audio_source(
        &state,
        &auth,
        item_id,
        query.user_id,
        query.media_source_id.as_deref(),
        &profile,
        query.max_streaming_bitrate,
        query.max_audio_channels,
        query.transcoding_container.as_deref(),
        query.transcoding_protocol,
    )
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("no media source for item {item_id}")))?;
    let source = &negotiated.source;

    if source.supports_direct_play
        && source.protocol == MediaProtocol::Http
        && query.enable_redirection.unwrap_or(true)
        && source.is_remote
        && query.enable_remote_media == Some(true)
        && let Some(path) = source.path.as_deref()
    {
        // A URL that is no valid header value fails, as Kestrel's would.
        let location = header::HeaderValue::from_str(path).map_err(|error| {
            ApiError::from(ferrofin_traits::error::ServiceError::backend(
                error.to_string(),
            ))
        })?;
        return Ok((StatusCode::FOUND, [(header::LOCATION, location)]).into_response());
    }

    // Upstream: "SupportsDirectStream actually means can direct play" here.
    let is_static = source.supports_direct_stream;
    let mut req = crate::handlers::hls::universal_request(item_id, &auth, &request);
    req.play_session_id = Some(negotiated.play_session_id.clone());
    req.media_source_id.clone_from(&query.media_source_id);
    req.device_id.clone_from(&query.device_id);
    req.audio_sample_rate = query.max_audio_sample_rate;
    req.max_audio_channels = query.max_audio_channels;
    req.max_audio_bit_depth = query.max_audio_bit_depth;
    req.start_time_ticks = query.start_time_ticks;
    req.context = Some(EncodingContext::Static);
    let reasons = negotiated.transcode_reasons;
    req.transcode_reasons = (!reasons.is_empty())
        .then(|| ferrofin_model::session::transcode_reasons_unique_names(reasons).join(","));
    // `SubtitleMethod` and `CopyTimestamps` are not carried: they only shape
    // subtitle and video arguments, and an audio transcode has neither.

    if !is_static && source.transcoding_sub_protocol == MediaStreamProtocol::hls {
        // Upstream reads `mediaSource.TranscodingContainer`, which is the
        // client's `transcodingContainer`; mpegts when HLS cannot segment it.
        req.segment_container = Some(
            query
                .transcoding_container
                .clone()
                .filter(|c| {
                    HLS_SEGMENT_CONTAINERS
                        .iter()
                        .any(|h| h.eq_ignore_ascii_case(c))
                })
                .unwrap_or_else(|| "ts".to_owned()),
        );
        req.audio_codec = if reasons == TranscodeReasons::CONTAINER_NOT_SUPPORTED {
            Some("copy".to_owned())
        } else {
            query.audio_codec.clone()
        };
        req.audio_bitrate = query.audio_bit_rate.or(query.max_streaming_bitrate);
        let playlist = state.hls.master_playlist(&req, true).await?;
        return Ok(crate::handlers::hls::universal_master_playlist_response(
            request.method(),
            playlist,
        ));
    }

    if is_static {
        // `AudioHelper.GetAudioStream` with `Static=true`: the version the
        // request names (the item's first by default), never a live stream.
        // Upstream passes the query's `mediaSourceId`, not the ranked source's
        // id, so with several versions and none named this is the first.
        let source = static_source(&state, item_id, req.media_source_id.as_deref(), None).await?;
        return serve_static_source(&state, source, request).await;
    }
    // The client's own container and codec, as upstream's `StreamingRequestDto`
    // (its `mediaSource.TranscodingContainer` is the query's, except for a
    // remote source forced to transcode, where it is the negotiated `mp3`
    // default the planner infers too). Either absent, the planner infers it
    // from the other, where upstream emits a bare `.` container (a Jellyfin
    // bug not ported).
    req.output_container
        .clone_from(&query.transcoding_container);
    req.audio_codec.clone_from(&query.audio_codec);
    req.audio_bitrate = query.audio_bit_rate.or(query.max_streaming_bitrate);
    req.audio_channels = query.max_audio_channels;
    crate::handlers::hls::transcode_stream_fallback(&state, item_id, true, req, request).await
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Audio/{itemId}/stream",
            get(get_audio_stream).head(get_audio_stream),
        )
        .route(
            "/Audio/{itemId}/{container}",
            get(get_audio_stream_by_container).head(get_audio_stream_by_container),
        )
        .route(
            "/Audio/{itemId}/universal",
            get(get_universal_audio_stream).head(get_universal_audio_stream),
        )
}
crate::query::query_parameters! {
    UniversalAudioQuery {"container" => ','} => [("get", "/Audio/{itemId}/universal")];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(query: &str) -> DeviceProfile {
        let uri = format!("/?{query}").parse().expect("uri");
        let query = Query::<UniversalAudioQuery>::try_from_uri(&uri)
            .expect("binds")
            .0;
        let containers: Vec<&str> = query
            .container
            .as_deref()
            .map(|c| c.split(',').filter(|c| !c.is_empty()).collect())
            .unwrap_or_default();
        universal_device_profile(&query, &containers)
    }

    /// `GetDeviceProfile`: a direct-play profile per `container|codec…` entry
    /// (an entry with no container is skipped where upstream throws), the
    /// mp3/HTTP transcoding default, and the caps as one codec profile over
    /// the raw container list.
    #[test]
    fn the_universal_profile_ports_get_device_profile() {
        let p = profile("container=opus,webm|opus|vorbis,|,mp3&transcodingAudioChannels=2");
        let direct: Vec<(&str, Option<&str>)> = p
            .direct_play_profiles
            .iter()
            .map(|d| (d.container.as_str(), d.audio_codec.as_deref()))
            .collect();
        assert_eq!(
            direct,
            [("opus", None), ("webm", Some("opus,vorbis")), ("mp3", None)]
        );
        assert!(
            p.direct_play_profiles
                .iter()
                .all(|d| d.profile_type == DlnaProfileType::Audio)
        );
        let [transcoding] = p.transcoding_profiles.as_slice() else {
            panic!("one transcoding profile");
        };
        assert_eq!(
            (
                transcoding.container.as_str(),
                transcoding.audio_codec.as_str()
            ),
            ("mp3", "mp3")
        );
        assert_eq!(transcoding.protocol, MediaStreamProtocol::http);
        assert_eq!(transcoding.context, EncodingContext::Streaming);
        assert_eq!(transcoding.max_audio_channels.as_deref(), Some("2"));
        assert!(p.codec_profiles.is_empty(), "no caps, no codec profile");
        assert!(profile("").direct_play_profiles.is_empty());

        let p = profile(
            "container=flac,mp3&maxAudioSampleRate=48000&maxAudioChannels=2\
             &transcodingContainer=ts&audioCodec=aac&transcodingProtocol=hls",
        );
        assert_eq!(p.transcoding_profiles[0].container, "ts");
        assert_eq!(p.transcoding_profiles[0].audio_codec, "aac");
        assert_eq!(p.transcoding_profiles[0].protocol, MediaStreamProtocol::hls);
        let [codec] = p.codec_profiles.as_slice() else {
            panic!("one codec profile");
        };
        assert_eq!(codec.codec_type, CodecType::Audio);
        assert_eq!(codec.container.as_deref(), Some("flac,mp3"));
        let conditions: Vec<(ProfileConditionValue, &str)> = codec
            .conditions
            .iter()
            .map(|c| {
                assert_eq!(c.condition, ProfileConditionType::LessThanEqual);
                assert!(!c.is_required);
                (c.property, c.value.as_str())
            })
            .collect();
        assert_eq!(
            conditions,
            [
                (ProfileConditionValue::AudioSampleRate, "48000"),
                (ProfileConditionValue::AudioChannels, "2"),
            ]
        );
    }
}
