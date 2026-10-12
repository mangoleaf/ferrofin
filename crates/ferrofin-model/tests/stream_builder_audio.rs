//! `StreamBuilder.GetOptimalAudioStream`: the audio-only decision — direct
//! play, container remux (direct stream) and audio transcoding — over profiles
//! built the way a music client declares them. Upstream's `StreamBuilderTests`
//! has no audio cases, so the expected values are read off the pinned
//! `StreamBuilder.cs` (`GetOptimalAudioStream`, `GetAudioDirectPlayProfile`,
//! `IsAudioDirectPlaySupported`/`IsAudioDirectStreamSupported`).

use ferrofin_model::data::MediaStreamProtocol;
use ferrofin_model::dlna::codec_profile::CodecProfile;
use ferrofin_model::dlna::direct_play_profile::DirectPlayProfile;
use ferrofin_model::dlna::enums::{
    CodecType, DlnaProfileType, EncodingContext, ProfileConditionType, ProfileConditionValue,
};
use ferrofin_model::dlna::profile_condition::ProfileCondition;
use ferrofin_model::dlna::stream_builder::StreamBuilder;
use ferrofin_model::dlna::transcoder_support::TranscoderSupport;
use ferrofin_model::dlna::transcoding_profile::TranscodingProfile;
use ferrofin_model::dlna::{DeviceProfile, MediaOptions};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::session::{PlayMethod, TranscodeReasons};
use uuid::Uuid;

/// A transcoder that can encode MP3 and AAC only.
struct Mp3AndAac;

impl TranscoderSupport for Mp3AndAac {
    fn can_encode_to_audio_codec(&self, codec: &str) -> bool {
        matches!(codec, "mp3" | "aac")
    }
    fn can_encode_to_subtitle_codec(&self, _codec: &str) -> bool {
        false
    }
    fn can_extract_subtitles(&self, _codec: &str) -> bool {
        false
    }
}

fn source(container: &str, codec: &str, channels: i32, bitrate: i32) -> MediaSourceInfo {
    MediaSourceInfo {
        id: Some("a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0".to_owned()),
        container: Some(container.to_owned()),
        bitrate: Some(bitrate),
        supports_direct_play: true,
        supports_direct_stream: true,
        supports_transcoding: true,
        media_streams: vec![MediaStream {
            index: 0,
            stream_type: MediaStreamType::Audio,
            codec: Some(codec.to_owned()),
            channels: Some(channels),
            bit_rate: Some(bitrate),
            ..MediaStream::default()
        }],
        ..MediaSourceInfo::default()
    }
}

fn direct_play(container: &str, audio_codec: Option<&str>) -> DirectPlayProfile {
    DirectPlayProfile {
        container: container.to_owned(),
        audio_codec: audio_codec.map(str::to_owned),
        video_codec: None,
        profile_type: DlnaProfileType::Audio,
    }
}

fn mp3_transcoding() -> TranscodingProfile {
    TranscodingProfile {
        container: "mp3".to_owned(),
        profile_type: DlnaProfileType::Audio,
        audio_codec: "mp3".to_owned(),
        protocol: MediaStreamProtocol::http,
        context: EncodingContext::Streaming,
        ..TranscodingProfile::default()
    }
}

fn options(profile: DeviceProfile, source: MediaSourceInfo) -> MediaOptions {
    let mut options = MediaOptions::new(profile);
    options.item_id = Uuid::parse_str("11D229B7-2D48-4B95-9F9B-49F6AB75E613").unwrap();
    options.media_source_id.clone_from(&source.id);
    options.media_sources = vec![source];
    options.device_id = Some("test-deviceId".to_owned());
    options.enable_direct_stream = false;
    options
}

fn optimal(options: &MediaOptions) -> Option<ferrofin_model::dlna::stream_info::StreamInfo> {
    StreamBuilder::new(&Mp3AndAac).get_optimal_audio_stream(options)
}

fn music_player() -> DeviceProfile {
    DeviceProfile {
        direct_play_profiles: vec![
            direct_play("mp3", Some("mp3")),
            direct_play("flac", None),
            direct_play("aac", None),
        ],
        transcoding_profiles: vec![mp3_transcoding()],
        ..DeviceProfile::default()
    }
}

#[test]
fn a_supported_container_and_codec_direct_plays() {
    let stream = optimal(&options(music_player(), source("mp3", "mp3", 2, 320_000))).unwrap();
    assert_eq!(stream.play_method, PlayMethod::DirectPlay);
    assert_eq!(stream.media_type, DlnaProfileType::Audio);
    assert_eq!(stream.container.as_deref(), Some("mp3"));
    assert!(stream.transcode_reasons.is_empty());
    // The URL is the audio controller's, a static progressive stream.
    let url = stream.to_url(None, None, None);
    assert!(
        url.starts_with("/audio/11d229b7-2d48-4b95-9f9b-49f6ab75e613/stream.mp3?"),
        "{url}"
    );
    assert!(url.contains("&Static=true"), "{url}");
}

#[test]
fn an_unsupported_container_and_codec_transcode_to_the_profile_codec() {
    let stream = optimal(&options(
        music_player(),
        source("ogg", "vorbis", 2, 160_000),
    ))
    .unwrap();
    assert_eq!(stream.play_method, PlayMethod::Transcode);
    assert_eq!(
        stream.transcode_reasons,
        TranscodeReasons::CONTAINER_NOT_SUPPORTED | TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED
    );
    assert_eq!(stream.container.as_deref(), Some("mp3"));
    assert_eq!(stream.audio_codecs, ["mp3"]);
    // The music streaming transcoding bitrate, never above the source's.
    assert_eq!(stream.audio_bitrate, Some(128_000));
    let url = stream.to_url(None, None, None);
    assert!(
        url.starts_with("/audio/11d229b7-2d48-4b95-9f9b-49f6ab75e613/stream.mp3?"),
        "{url}"
    );
    assert!(url.contains("&AudioCodec=mp3"), "{url}");
}

#[test]
fn a_bitrate_over_the_limit_transcodes_with_that_reason() {
    let mut options = options(music_player(), source("flac", "flac", 2, 900_000));
    options.max_bitrate = Some(256_000);
    let stream = optimal(&options).unwrap();
    assert_eq!(stream.play_method, PlayMethod::Transcode);
    assert!(
        stream
            .transcode_reasons
            .contains(TranscodeReasons::CONTAINER_BITRATE_EXCEEDS_LIMIT),
        "{:?}",
        stream.transcode_reasons
    );
    assert_eq!(stream.audio_bitrate, Some(128_000));
}

#[test]
fn a_requested_transcoding_bitrate_is_capped_by_the_configured_limit() {
    let mut options = options(music_player(), source("ogg", "vorbis", 2, 160_000));
    options.audio_transcoding_bitrate = Some(320_000);
    options.max_bitrate = Some(192_000);
    assert_eq!(optimal(&options).unwrap().audio_bitrate, Some(192_000));
}

#[test]
fn a_codec_condition_the_audio_fails_sends_a_direct_play_to_transcoding() {
    let mut profile = music_player();
    profile.codec_profiles = vec![CodecProfile {
        codec_type: CodecType::Audio,
        conditions: vec![ProfileCondition::with_required(
            ProfileConditionType::LessThanEqual,
            ProfileConditionValue::AudioChannels,
            "2".to_owned(),
            true,
        )],
        ..CodecProfile::default()
    }];
    let stream = optimal(&options(profile, source("flac", "flac", 6, 900_000))).unwrap();
    assert_eq!(stream.play_method, PlayMethod::Transcode);
    assert!(
        stream
            .transcode_reasons
            .contains(TranscodeReasons::AUDIO_CHANNELS_NOT_SUPPORTED),
        "{:?}",
        stream.transcode_reasons
    );
    assert_eq!(stream.container.as_deref(), Some("mp3"));
}

#[test]
fn a_container_the_device_lacks_but_whose_codec_it_names_is_remuxed() {
    // Declared as the bare codec `aac` (an ADTS stream): the item's `mp4`
    // container is not a direct play, its AAC is a direct stream.
    let profile = DeviceProfile {
        direct_play_profiles: vec![direct_play("aac", None)],
        transcoding_profiles: vec![mp3_transcoding()],
        ..DeviceProfile::default()
    };
    let stream = optimal(&options(profile, source("mp4", "aac", 2, 256_000))).unwrap();
    assert_eq!(stream.play_method, PlayMethod::DirectStream);
    assert_eq!(stream.container.as_deref(), Some("ts"));
    assert_eq!(
        stream.transcode_reasons,
        TranscodeReasons::CONTAINER_NOT_SUPPORTED
    );
}

#[test]
fn an_hls_remux_needs_a_codec_hls_carries() {
    let profile = DeviceProfile {
        direct_play_profiles: vec![direct_play("flac", None)],
        transcoding_profiles: vec![mp3_transcoding()],
        ..DeviceProfile::default()
    };
    let mut hls = source("mp4", "flac", 2, 900_000);
    hls.transcoding_sub_protocol = MediaStreamProtocol::hls;
    // `ts` segments do not carry FLAC: the remux is refused and it transcodes.
    let stream = optimal(&options(profile.clone(), hls)).unwrap();
    assert_eq!(stream.play_method, PlayMethod::Transcode);
    assert!(
        stream
            .transcode_reasons
            .contains(TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED)
    );
    // …while a profile that names fMP4 as its container may carry it.
    let mut fmp4 = profile;
    fmp4.direct_play_profiles = vec![direct_play("mp4", Some("flac")), direct_play("flac", None)];
    let mut hls = source("m4a", "flac", 2, 900_000);
    hls.transcoding_sub_protocol = MediaStreamProtocol::hls;
    let stream = optimal(&options(fmp4, hls)).unwrap();
    assert_eq!(stream.play_method, PlayMethod::DirectStream);
    assert_eq!(stream.container.as_deref(), Some("mp4"));
}

#[test]
fn matroska_audio_is_not_direct_played_for_a_webm_only_device() {
    // ffprobe names a Matroska file `mkv,webm`; a device that declared only
    // `webm` must not be sent it as a direct play (jellyfin#13344) — here its
    // Opus is remuxed instead.
    let webm_only = DeviceProfile {
        direct_play_profiles: vec![direct_play("webm", Some("opus"))],
        transcoding_profiles: vec![mp3_transcoding()],
        ..DeviceProfile::default()
    };
    let stream = optimal(&options(webm_only, source("mkv,webm", "opus", 2, 128_000))).unwrap();
    assert_eq!(stream.play_method, PlayMethod::DirectStream);
    assert_eq!(
        stream.transcode_reasons,
        TranscodeReasons::CONTAINER_NOT_SUPPORTED
    );
    // A device that also declares `mkv` plays it as it is.
    let both = DeviceProfile {
        direct_play_profiles: vec![direct_play("mkv,webm", Some("opus"))],
        transcoding_profiles: vec![mp3_transcoding()],
        ..DeviceProfile::default()
    };
    let stream = optimal(&options(both, source("mkv,webm", "opus", 2, 128_000))).unwrap();
    assert_eq!(stream.play_method, PlayMethod::DirectPlay);
}

#[test]
fn forced_methods_skip_the_negotiation() {
    let mut forced = options(music_player(), source("ogg", "vorbis", 2, 160_000));
    forced.force_direct_play = true;
    let stream = optimal(&forced).unwrap();
    assert_eq!(stream.play_method, PlayMethod::DirectPlay);
    forced.force_direct_play = false;
    forced.force_direct_stream = true;
    assert_eq!(
        optimal(&forced).unwrap().play_method,
        PlayMethod::DirectStream
    );
}

#[test]
fn nothing_is_offered_without_an_audio_stream_or_a_permitted_transcode() {
    let mut no_audio = source("mp3", "mp3", 2, 320_000);
    no_audio.media_streams.clear();
    assert!(optimal(&options(music_player(), no_audio)).is_none());

    let mut forbidden = source("ogg", "vorbis", 2, 160_000);
    forbidden.supports_transcoding = false;
    assert!(optimal(&options(music_player(), forbidden)).is_none());
}

#[test]
fn a_profile_the_transcoder_cannot_encode_leaves_a_bare_transcode() {
    let mut profile = music_player();
    profile.transcoding_profiles[0].audio_codec = "opus".to_owned();
    profile.transcoding_profiles[0].container = "opus".to_owned();
    let stream = optimal(&options(profile, source("ogg", "vorbis", 2, 160_000))).unwrap();
    assert_eq!(stream.play_method, PlayMethod::Transcode);
    assert!(stream.container.is_none(), "{:?}", stream.container);
    assert!(stream.audio_codecs.is_empty());
}
