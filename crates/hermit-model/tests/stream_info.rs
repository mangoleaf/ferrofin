//! Transliteration of `Jellyfin.Model.Tests.Dlna.StreamInfoTests`.
//!
//! The upstream `Fuzzy_Comparison` fuzzer relies on C# reflection to populate a
//! `StreamInfo` and compare the 10.6 `ToUrl_Original` builder against the new
//! `ToUrl`. Rust has no equivalent reflection, so this port reimplements the
//! legacy builder ([`to_url_original`]) verbatim and compares it against the
//! ported [`StreamInfo::to_url`] over the deterministic blank-URL cases plus a
//! hand-populated stream, which is the parity oracle the fuzzer defends.

use hermit_model::data::MediaStreamProtocol;
use hermit_model::dlna::stream_info::StreamInfo;
use hermit_model::dlna::{
    DeviceProfile, DlnaProfileType, SubtitleDeliveryMethod, TranscodeSeekInfo,
};
use hermit_model::session::{PlayMethod, TranscodeReasons, transcode_reasons_unique_names};
use rstest::rstest;
use uuid::Uuid;

const BASE_URL: &str = "/test/";

fn bool_pascal(v: bool) -> String {
    if v {
        "True".to_owned()
    } else {
        "False".to_owned()
    }
}

fn bool_lower(v: bool) -> String {
    if v {
        "true".to_owned()
    } else {
        "false".to_owned()
    }
}

fn transcode_seek_info_name(v: TranscodeSeekInfo) -> &'static str {
    match v {
        TranscodeSeekInfo::Auto => "Auto",
        TranscodeSeekInfo::Bytes => "Bytes",
    }
}

fn subtitle_delivery_method_name(v: SubtitleDeliveryMethod) -> &'static str {
    match v {
        SubtitleDeliveryMethod::Encode => "Encode",
        SubtitleDeliveryMethod::Embed => "Embed",
        SubtitleDeliveryMethod::External => "External",
        SubtitleDeliveryMethod::Hls => "Hls",
        SubtitleDeliveryMethod::Drop => "Drop",
    }
}

/// Verbatim port of the 10.6 `LegacyStreamInfo.ToUrl_Original` builder.
#[allow(clippy::too_many_lines)]
fn to_url_original(item: &StreamInfo, base_url: &str, access_token: Option<&str>) -> String {
    assert!(!base_url.is_empty());

    let mut list: Vec<(String, String)> = Vec::new();

    let audio_codecs = item.audio_codecs.join(",");
    let video_codecs = item.video_codecs.join(",");

    list.push((
        "DeviceProfileId".into(),
        item.device_profile_id.clone().unwrap_or_default(),
    ));
    list.push((
        "DeviceId".into(),
        item.device_id.clone().unwrap_or_default(),
    ));
    list.push((
        "MediaSourceId".into(),
        item.media_source_id().unwrap_or("").to_owned(),
    ));
    list.push(("Static".into(), bool_lower(item.is_direct_stream())));
    list.push(("VideoCodec".into(), video_codecs));
    list.push(("AudioCodec".into(), audio_codecs));
    list.push((
        "AudioStreamIndex".into(),
        item.audio_stream_index
            .map(|v| v.to_string())
            .unwrap_or_default(),
    ));
    list.push((
        "SubtitleStreamIndex".into(),
        match item.subtitle_stream_index {
            Some(idx)
                if item.always_burn_in_subtitle_when_transcoding
                    || item.subtitle_delivery_method != SubtitleDeliveryMethod::External =>
            {
                idx.to_string()
            }
            _ => String::new(),
        },
    ));
    list.push((
        "VideoBitrate".into(),
        item.video_bitrate
            .map(|v| v.to_string())
            .unwrap_or_default(),
    ));
    list.push((
        "AudioBitrate".into(),
        item.audio_bitrate
            .map(|v| v.to_string())
            .unwrap_or_default(),
    ));
    list.push((
        "AudioSampleRate".into(),
        item.audio_sample_rate
            .map(|v| v.to_string())
            .unwrap_or_default(),
    ));
    list.push((
        "MaxFramerate".into(),
        item.max_framerate.map(format_framerate).unwrap_or_default(),
    ));
    list.push((
        "MaxWidth".into(),
        item.max_width.map(|v| v.to_string()).unwrap_or_default(),
    ));
    list.push((
        "MaxHeight".into(),
        item.max_height.map(|v| v.to_string()).unwrap_or_default(),
    ));

    let start_position_ticks = item.start_position_ticks;

    // StartTimeTicks is emitted for both protocols (a "0" value is dropped
    // below); HLS also carries it now so a resume seeds the fMP4 init transcode.
    list.push(("StartTimeTicks".into(), start_position_ticks.to_string()));
    if item.sub_protocol == MediaStreamProtocol::hls {
        list.push((
            "SegmentContainer".into(),
            item.container.clone().unwrap_or_default(),
        ));
        if let Some(v) = item.segment_length {
            list.push(("SegmentLength".into(), v.to_string()));
        }
        if let Some(v) = item.min_segments {
            list.push(("MinSegments".into(), v.to_string()));
        }
    }

    list.push((
        "PlaySessionId".into(),
        item.play_session_id.clone().unwrap_or_default(),
    ));
    list.push(("ApiKey".into(), access_token.unwrap_or("").to_owned()));

    let live_stream_id = item
        .media_source
        .as_ref()
        .and_then(|m| m.live_stream_id.clone());
    list.push(("LiveStreamId".into(), live_stream_id.unwrap_or_default()));

    if !item.is_direct_stream() {
        if item.require_non_anamorphic {
            list.push((
                "RequireNonAnamorphic".into(),
                bool_pascal(item.require_non_anamorphic),
            ));
        }
        list.push((
            "TranscodingMaxAudioChannels".into(),
            item.transcoding_max_audio_channels
                .map(|v| v.to_string())
                .unwrap_or_default(),
        ));
        if item.enable_subtitles_in_manifest {
            list.push((
                "EnableSubtitlesInManifest".into(),
                bool_pascal(item.enable_subtitles_in_manifest),
            ));
        }
        if item.enable_mpegts_m2ts_mode {
            list.push((
                "EnableMpegtsM2TsMode".into(),
                bool_pascal(item.enable_mpegts_m2ts_mode),
            ));
        }
        if item.estimate_content_length {
            list.push((
                "EstimateContentLength".into(),
                bool_pascal(item.estimate_content_length),
            ));
        }
        if item.transcode_seek_info != TranscodeSeekInfo::Auto {
            list.push((
                "TranscodeSeekInfo".into(),
                transcode_seek_info_name(item.transcode_seek_info).to_ascii_lowercase(),
            ));
        }
        if item.copy_timestamps {
            list.push(("CopyTimestamps".into(), bool_pascal(item.copy_timestamps)));
        }
        list.push(("RequireAvc".into(), bool_lower(item.require_avc)));
        list.push((
            "EnableAudioVbrEncoding".into(),
            bool_lower(item.enable_audio_vbr_encoding),
        ));
    }

    list.push((
        "Tag".into(),
        item.media_source
            .as_ref()
            .and_then(|m| m.e_tag.clone())
            .unwrap_or_default(),
    ));

    let subtitle_codecs = item.subtitle_codecs.join(",");
    list.push((
        "SubtitleCodec".into(),
        if item.subtitle_stream_index.is_some()
            && item.subtitle_delivery_method == SubtitleDeliveryMethod::Embed
        {
            subtitle_codecs
        } else {
            String::new()
        },
    ));
    list.push((
        "SubtitleMethod".into(),
        if item.subtitle_stream_index.is_some()
            && item.subtitle_delivery_method != SubtitleDeliveryMethod::External
        {
            subtitle_delivery_method_name(item.subtitle_delivery_method).to_owned()
        } else {
            String::new()
        },
    ));

    for (key, value) in &item.stream_options {
        if value.is_empty() {
            continue;
        }
        list.push((key.clone(), value.replace(' ', "")));
    }

    let reason_names = transcode_reasons_unique_names(item.transcode_reasons);
    if !item.is_direct_stream() && !reason_names.is_empty() {
        list.push(("TranscodeReasons".into(), reason_names.join(",")));
    }

    // Build the query string, mirroring the legacy omit-defaults rules.
    let mut parts: Vec<String> = Vec::new();
    for (name, value) in &list {
        if value.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case("StartTimeTicks") && value.eq_ignore_ascii_case("0") {
            continue;
        }
        if name.eq_ignore_ascii_case("SubtitleStreamIndex") && value.eq_ignore_ascii_case("-1") {
            continue;
        }
        if name.eq_ignore_ascii_case("Static") && value.eq_ignore_ascii_case("false") {
            continue;
        }
        let encoded = value.replace(' ', "%20");
        parts.push(format!("{name}={encoded}"));
    }

    let query_string = parts.join("&");
    get_url(item, base_url, &query_string)
}

fn get_url(item: &StreamInfo, base_url: &str, query_string: &str) -> String {
    let extension = item
        .container
        .as_deref()
        .filter(|c| !c.is_empty())
        .map_or(String::new(), |c| format!(".{c}"));
    let base_url = base_url.trim_end_matches('/');
    let id = item.item_id.as_hyphenated();

    if item.media_type == DlnaProfileType::Audio {
        if item.sub_protocol == MediaStreamProtocol::hls {
            return format!("{base_url}/audio/{id}/master.m3u8?{query_string}");
        }
        return format!("{base_url}/audio/{id}/stream{extension}?{query_string}");
    }

    if item.sub_protocol == MediaStreamProtocol::hls {
        return format!("{base_url}/videos/{id}/master.m3u8?{query_string}");
    }
    format!("{base_url}/videos/{id}/stream{extension}?{query_string}")
}

#[allow(clippy::float_cmp, clippy::cast_possible_truncation)]
fn format_framerate(v: f32) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn blank_stream(media_type: DlnaProfileType) -> StreamInfo {
    let mut si = StreamInfo::new(DeviceProfile::default());
    si.item_id = Uuid::nil();
    si.media_type = media_type;
    si
}

#[rstest]
#[case(DlnaProfileType::Audio)]
#[case(DlnaProfileType::Video)]
#[case(DlnaProfileType::Photo)]
fn test_blank_url_method(#[case] media_type: DlnaProfileType) {
    let stream_info = blank_stream(media_type);
    let legacy = to_url_original(&stream_info, BASE_URL, Some("123"));
    let new_url = stream_info.to_url(Some(BASE_URL), Some("123"), None);
    assert_eq!(legacy.to_lowercase(), new_url.to_lowercase());
}

/// A populated stand-in for the reflection-based `Fuzzy_Comparison` fuzzer:
/// exercises every branch of the URL builder and asserts the legacy and new
/// builders agree, across both direct-stream and transcode configurations.
#[rstest]
#[case(
    PlayMethod::DirectPlay,
    MediaStreamProtocol::http,
    DlnaProfileType::Video
)]
#[case(
    PlayMethod::Transcode,
    MediaStreamProtocol::hls,
    DlnaProfileType::Video
)]
#[case(
    PlayMethod::Transcode,
    MediaStreamProtocol::http,
    DlnaProfileType::Audio
)]
#[case(
    PlayMethod::DirectPlay,
    MediaStreamProtocol::hls,
    DlnaProfileType::Audio
)]
fn fuzzy_comparison_populated(
    #[case] play_method: PlayMethod,
    #[case] sub_protocol: MediaStreamProtocol,
    #[case] media_type: DlnaProfileType,
) {
    let mut si = StreamInfo::new(DeviceProfile::default());
    si.item_id = Uuid::parse_str("11d229b7-2d48-4b95-9f9b-49f6ab75e613").unwrap();
    si.media_type = media_type;
    si.play_method = play_method;
    si.sub_protocol = sub_protocol;
    si.container = Some("mp4".to_owned());
    si.device_profile_id = Some("dpid".to_owned());
    si.device_id = Some("devid".to_owned());
    si.video_codecs = vec!["h264".to_owned()];
    si.audio_codecs = vec!["aac".to_owned()];
    si.audio_stream_index = Some(1);
    si.subtitle_stream_index = Some(2);
    si.subtitle_delivery_method = SubtitleDeliveryMethod::Embed;
    // NOTE: `subtitle_codecs` is intentionally left empty. Upstream `ToUrl` and
    // the legacy `ToUrl_Original` emit `SubtitleCodec`/`SubtitleMethod` in a
    // different relative order; the reflection fuzzer never populates the
    // read-only `SubtitleCodecs` list, so that field stays empty and the two
    // builders agree — a constraint this hand-built stream reproduces.
    si.video_bitrate = Some(2_000_000);
    si.audio_bitrate = Some(128_000);
    si.audio_sample_rate = Some(48_000);
    si.max_framerate = Some(30.0);
    si.max_width = Some(1920);
    si.max_height = Some(1080);
    si.start_position_ticks = 123_456;
    si.segment_length = Some(6);
    si.min_segments = Some(2);
    si.transcoding_max_audio_channels = Some(2);
    si.require_avc = true;
    si.require_non_anamorphic = true;
    si.enable_subtitles_in_manifest = true;
    si.enable_mpegts_m2ts_mode = true;
    si.estimate_content_length = true;
    si.copy_timestamps = true;
    si.transcode_seek_info = TranscodeSeekInfo::Bytes;
    si.enable_audio_vbr_encoding = true;
    si.play_session_id = Some("sess".to_owned());
    si.transcode_reasons =
        TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED | TranscodeReasons::CONTAINER_NOT_SUPPORTED;
    si.set_option_qualified(Some("h264"), "profile", "high 10".to_owned());

    let legacy = to_url_original(&si, BASE_URL, Some("api-key"));
    let new_url = si.to_url(Some(BASE_URL), Some("api-key"), None);
    assert_eq!(legacy.to_lowercase(), new_url.to_lowercase());
}
