//! The stream maps and the extra ffmpeg inputs an external subtitle or audio
//! track needs: `EncodingHelper.GetMapArgs` and the tail of
//! `GetInputArgument`.
//!
//! The `GetMapArgs_*` and `GetInputArgument_VobSub_UsesCorrectPath` cases are
//! transliterated from upstream's
//! `tests/Jellyfin.Controller.Tests/MediaEncoding/EncodingHelperTests.cs`
//! (v12.2), whose expected values are the oracle; the rest pin what the maps
//! assume about the inputs.

use ferrofin_mediaencoding::encoding_helper::helper::external_stream_input_arguments;
use ferrofin_mediaencoding::{
    BaseEncodingJobOptions, EncodingHelper, EncodingJobInfo, FfmpegVersion, NoOptionalEncoders,
    TranscodeDisplayNames,
};
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::media_encoding::TranscodingJobType;
use rstest::rstest;

fn stream(index: i32, stream_type: MediaStreamType, codec: &str) -> MediaStream {
    MediaStream {
        index,
        stream_type,
        codec: Some(codec.to_owned()),
        ..MediaStream::default()
    }
}

fn external_subtitle(index: i32, codec: &str, path: &str) -> MediaStream {
    MediaStream {
        is_external: true,
        supports_external_stream: true,
        path: Some(path.to_owned()),
        ..stream(index, MediaStreamType::Subtitle, codec)
    }
}

/// Upstream's `BuildState`: a progressive job over an mkv with video 0 and
/// audio 1, plus `additional` (or just the subtitle), the method defaulting
/// to `Drop`.
fn build_state(
    subtitle: Option<MediaStream>,
    method: Option<SubtitleDeliveryMethod>,
    additional: Option<Vec<MediaStream>>,
) -> EncodingJobInfo {
    let video = stream(0, MediaStreamType::Video, "h264");
    let audio = stream(1, MediaStreamType::Audio, "aac");
    let mut streams = vec![video.clone(), audio.clone()];
    match (additional, &subtitle) {
        (Some(additional), _) => streams.extend(additional),
        (None, Some(subtitle)) => streams.push(subtitle.clone()),
        (None, None) => {}
    }
    EncodingJobInfo {
        display: TranscodeDisplayNames::default(),
        base_request: BaseEncodingJobOptions::default(),
        video_stream: Some(video),
        audio_stream: Some(audio),
        subtitle_stream: subtitle,
        media_source: MediaSourceInfo {
            container: Some("mkv".to_owned()),
            media_streams: streams,
            ..MediaSourceInfo::default()
        },
        output_video_codec: None,
        output_audio_codec: None,
        output_video_bitrate: None,
        output_audio_bitrate: None,
        output_audio_channels: None,
        output_container: None,
        output_video_sync: None,
        output_file_path: String::new(),
        input_container: None,
        is_input_video: true,
        subtitle_delivery_method: method.unwrap_or(SubtitleDeliveryMethod::Drop),
        run_time_ticks: None,
        transcoding_type: TranscodingJobType::Progressive,
        supported_video_codecs: Vec::new(),
        supported_audio_codecs: Vec::new(),
        supported_subtitle_codecs: Vec::new(),
        segment_length_secs: 0,
        wait_for_path: None,
        segment_container: None,
        play_session_id: None,
        device_id: None,
    }
}

fn map_args(state: &EncodingJobInfo) -> String {
    EncodingHelper::with_processor_count(NoOptionalEncoders, 8).map_args(state, false)
}

#[test]
fn get_map_args_no_subtitle_excludes_all_subs() {
    let args = map_args(&build_state(None, None, None));
    assert!(args.contains("-map -0:s"), "{args}");
    assert!(!args.contains("-map 1:"), "{args}");
}

#[test]
fn get_map_args_internal_srt_maps_from_primary_input() {
    let sub = stream(2, MediaStreamType::Subtitle, "srt");
    let args = map_args(&build_state(
        Some(sub),
        Some(SubtitleDeliveryMethod::Embed),
        None,
    ));
    assert!(args.contains("-map 0:2"), "{args}");
    assert!(!args.contains("-map 1:"), "{args}");
}

#[test]
fn get_map_args_internal_sub_at_higher_index_maps_correct_index() {
    let sub0 = stream(2, MediaStreamType::Subtitle, "srt");
    let sub1 = stream(3, MediaStreamType::Subtitle, "ass");
    let args = map_args(&build_state(
        Some(sub1.clone()),
        Some(SubtitleDeliveryMethod::Embed),
        Some(vec![sub0, sub1]),
    ));
    assert!(args.contains("-map 0:3"), "{args}");
}

#[test]
fn get_map_args_external_srt_maps_first_stream_from_input_1() {
    let sub = external_subtitle(2, "srt", "/media/movie.en.srt");
    let args = map_args(&build_state(
        Some(sub),
        Some(SubtitleDeliveryMethod::Embed),
        None,
    ));
    assert!(args.contains("-map 1:0"), "{args}");
}

/// Two separate .srt files: selecting the second one still maps 1:0,
/// because only the selected file is fed as ffmpeg input 1.
#[test]
fn get_map_args_second_external_srt_still_maps_1_colon_0() {
    let ext1 = external_subtitle(2, "srt", "/media/movie.en.srt");
    let ext2 = external_subtitle(3, "srt", "/media/movie.fr.srt");
    let args = map_args(&build_state(
        Some(ext2.clone()),
        Some(SubtitleDeliveryMethod::Embed),
        Some(vec![ext1, ext2]),
    ));
    assert!(args.contains("-map 1:0"), "{args}");
}

#[test]
fn get_map_args_mks_first_track_maps_in_file_index_0() {
    let mks0 = external_subtitle(2, "subrip", "/media/movie.mks");
    let mks1 = external_subtitle(3, "ass", "/media/movie.mks");
    let args = map_args(&build_state(
        Some(mks0.clone()),
        Some(SubtitleDeliveryMethod::Embed),
        Some(vec![mks0, mks1]),
    ));
    assert!(args.contains("-map 1:0"), "{args}");
}

#[test]
fn get_map_args_mks_second_track_maps_in_file_index_1() {
    let mks0 = external_subtitle(2, "subrip", "/media/movie.mks");
    let mks1 = external_subtitle(3, "ass", "/media/movie.mks");
    let mks2 = external_subtitle(4, "subrip", "/media/movie.mks");
    let args = map_args(&build_state(
        Some(mks1.clone()),
        Some(SubtitleDeliveryMethod::Embed),
        Some(vec![mks0, mks1, mks2]),
    ));
    assert!(args.contains("-map 1:1"), "{args}");
}

/// A VobSub `.sub` is read through the `.idx` beside it, when there is one,
/// whether it is embedded or burned in.
#[rstest]
#[case(SubtitleDeliveryMethod::Embed, true, "movie.idx")]
#[case(SubtitleDeliveryMethod::Encode, true, "movie.idx")]
#[case(SubtitleDeliveryMethod::Embed, false, "movie.sub")]
#[case(SubtitleDeliveryMethod::Encode, false, "movie.sub")]
fn get_input_argument_vob_sub_uses_correct_path(
    #[case] method: SubtitleDeliveryMethod,
    #[case] create_idx_file: bool,
    #[case] expected_file_name: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let sub_file = dir.path().join("movie.sub");
    std::fs::write(&sub_file, "dummy").unwrap();
    if create_idx_file {
        std::fs::write(dir.path().join("movie.idx"), "dummy").unwrap();
    }
    let sub = external_subtitle(2, "dvdsub", &sub_file.to_string_lossy());
    let input_args =
        external_stream_input_arguments(&build_state(Some(sub), Some(method), None), None, None);
    assert!(
        input_args.last().unwrap().ends_with(expected_file_name),
        "{input_args:?}"
    );
}

/// The `.sub` extension is matched ignoring case, as upstream's
/// `OrdinalIgnoreCase` does, and the `.idx` keeps `ChangeExtension`'s
/// spelling.
#[test]
fn an_upper_case_vob_sub_is_read_through_its_idx() {
    let dir = tempfile::tempdir().unwrap();
    let sub_file = dir.path().join("MOVIE.SUB");
    std::fs::write(&sub_file, "dummy").unwrap();
    std::fs::write(dir.path().join("MOVIE.idx"), "dummy").unwrap();
    let sub = external_subtitle(2, "dvdsub", &sub_file.to_string_lossy());
    let input_args = external_stream_input_arguments(
        &build_state(Some(sub), Some(SubtitleDeliveryMethod::Encode), None),
        None,
        None,
    );
    assert!(
        input_args.last().unwrap().ends_with("MOVIE.idx"),
        "{input_args:?}"
    );
}

/// The subtitle input reads like the media file (`-analyzeduration`,
/// `-probesize`), seeks with it, and a burned-in bitmap is decoded onto the
/// canvas it was authored for; the `-i` names it as a `file:` URL.
#[test]
fn a_burned_external_bitmap_is_input_1_seeked_and_on_its_canvas() {
    let mut sub = external_subtitle(2, "PGSSUB", "/media/movie en.sup");
    sub.width = Some(1920);
    sub.height = Some(1080);
    let mut state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Encode), None);
    state.media_source.analyze_duration_ms = Some(3000);
    assert_eq!(
        external_stream_input_arguments(&state, Some("00:01:02.500"), None),
        [
            "-analyzeduration",
            "3000000",
            "-probesize",
            "1G",
            "-ss",
            "00:01:02.500",
            "-canvas_size",
            "1920x1080",
            "-i",
            "file:/media/movie en.sup",
        ]
    );
    let args = map_args(&state);
    assert!(args.contains(" -map 1:0 -sn"), "{args}");
}

/// Where ffmpeg can sync it to the media file's clock, a burned-in bitmap
/// sidecar is read from its start instead: the overlay draws each bitmap at
/// its own time, and a seeked, synced one is never drawn.
#[test]
fn a_burned_external_bitmap_is_read_from_its_start_on_the_media_files_clock() {
    let mut sub = external_subtitle(2, "PGSSUB", "/media/movie.en.sup");
    sub.width = Some(1920);
    sub.height = Some(1080);
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Encode), None);
    assert_eq!(
        external_stream_input_arguments(
            &state,
            Some("00:01:02.500"),
            Some(FfmpegVersion::new(7, 0))
        ),
        [
            "-analyzeduration",
            "200M",
            "-probesize",
            "1G",
            "-canvas_size",
            "1920x1080",
            "-isync",
            "0",
            "-i",
            "file:/media/movie.en.sup",
        ]
    );
}

/// The embedded half of that pair: an embedded bitmap sidecar keeps the
/// media file's seek (its packets are copied, and an unseeked one would
/// carry negative timestamps into the muxer) and is synced, with no canvas;
/// with no seek it is synced all the same.
#[rstest]
#[case::seeked(Some("10"), &["-ss", "10", "-isync", "0"])]
#[case::from_the_start(None, &["-isync", "0"])]
fn an_embedded_external_bitmap_keeps_the_seek_and_is_synced(
    #[case] seek: Option<&str>,
    #[case] timing: &[&str],
) {
    let sub = external_subtitle(2, "PGSSUB", "/media/movie.en.sup");
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    let mut expected = vec!["-analyzeduration", "200M", "-probesize", "1G"];
    expected.extend(timing);
    expected.extend(["-i", "file:/media/movie.en.sup"]);
    assert_eq!(
        external_stream_input_arguments(&state, seek, Some(FfmpegVersion::new(7, 0))),
        expected
    );
}

/// An embedded external track is input 1 too, with no canvas: it is copied
/// or converted, not drawn.
#[test]
fn an_embedded_external_text_track_is_input_1_without_a_canvas() {
    let sub = external_subtitle(2, "srt", "/media/movie.en.srt");
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    assert_eq!(
        external_stream_input_arguments(&state, None, None),
        [
            "-analyzeduration",
            "200M",
            "-probesize",
            "1G",
            "-i",
            "file:/media/movie.en.srt",
        ]
    );
}

/// A burned-in external text file is read by the `subtitles` filter, and a
/// track the client fetches itself is no input at all.
#[rstest]
#[case::burned_text("srt", SubtitleDeliveryMethod::Encode)]
#[case::external_delivery("PGSSUB", SubtitleDeliveryMethod::External)]
#[case::hls_delivery("srt", SubtitleDeliveryMethod::Hls)]
#[case::dropped("PGSSUB", SubtitleDeliveryMethod::Drop)]
fn other_external_subtitles_add_no_input(
    #[case] codec: &str,
    #[case] method: SubtitleDeliveryMethod,
) {
    let sub = external_subtitle(2, codec, "/media/movie.en.sub");
    let state = build_state(Some(sub), Some(method), None);
    assert!(
        external_stream_input_arguments(&state, Some("10"), Some(FfmpegVersion::new(7, 0)))
            .is_empty()
    );
}

/// Without `-copyts` every input's timeline starts at its own first packet,
/// so each external input is put on the media file's clock, by the `-isync`
/// ffmpeg 5.1 brought; an older or unknown ffmpeg gets no option it would
/// refuse.
#[rstest]
#[case::with_isync(Some(FfmpegVersion::new(5, 1)), true)]
#[case::newer(Some(FfmpegVersion::with_build(7, 0, 1)), true)]
#[case::before_isync(Some(FfmpegVersion::new(5, 0)), false)]
#[case::unknown(None, false)]
fn external_inputs_are_synced_to_the_media_files_clock(
    #[case] version: Option<FfmpegVersion>,
    #[case] synced: bool,
) {
    let sub = external_subtitle(2, "srt", "/media/movie.en.srt");
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    let inputs = external_stream_input_arguments(&state, Some("10"), version);
    let mut expected = vec!["-analyzeduration", "200M", "-probesize", "1G", "-ss", "10"];
    if synced {
        expected.extend(["-isync", "0"]);
    }
    expected.extend(["-i", "file:/media/movie.en.srt"]);
    assert_eq!(inputs, expected);
}

/// An external audio track follows the subtitle input, seeked alike, so the
/// map's `2:` (after a subtitle) or `1:` (alone) names it.
#[rstest]
#[case::after_a_subtitle(true, "2")]
#[case::alone(false, "1")]
fn an_external_audio_track_is_the_next_input(#[case] with_subtitle: bool, #[case] input: &str) {
    let audio = MediaStream {
        is_external: true,
        path: Some("/media/movie.commentary.mka".to_owned()),
        ..stream(3, MediaStreamType::Audio, "aac")
    };
    let sub = with_subtitle.then(|| external_subtitle(2, "srt", "/media/movie.en.srt"));
    let mut additional = vec![audio.clone()];
    additional.extend(sub.clone());
    let mut state = build_state(sub, Some(SubtitleDeliveryMethod::Embed), Some(additional));
    state.audio_stream = Some(audio);

    let inputs =
        external_stream_input_arguments(&state, Some("10"), Some(FfmpegVersion::new(7, 0)));
    assert_eq!(
        inputs[inputs.len() - 6..],
        [
            "-ss",
            "10",
            "-isync",
            "0",
            "-i",
            "/media/movie.commentary.mka"
        ]
    );
    assert_eq!(
        inputs.iter().filter(|a| *a == "-i").count(),
        if with_subtitle { 2 } else { 1 }
    );
    let args = map_args(&state);
    assert!(args.contains(&format!(" -map {input}:0")), "{args}");
}
