//! The stream maps and the extra ffmpeg inputs an external subtitle or audio
//! track needs: `EncodingHelper.GetMapArgs` and the tail of
//! `GetInputArgument`.
//!
//! The `GetMapArgs_*` and `GetInputArgument_VobSub_UsesCorrectPath` cases are
//! transliterated from upstream's
//! `tests/Jellyfin.Controller.Tests/MediaEncoding/EncodingHelperTests.cs`
//! (v12.2), whose expected values are the oracle; the rest pin what the maps
//! assume about the inputs.

use ferrofin_mediaencoding::encoder::media_encoder::InputAnalysis;
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
        trimmed_subtitle_path: None,
        transcode_reasons: ferrofin_model::session::TranscodeReasons::empty(),
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
    let input_args = external_stream_input_arguments(
        &build_state(Some(sub), Some(method), None),
        None,
        None,
        &InputAnalysis::default(),
    );
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
        &InputAnalysis::default(),
    );
    assert!(
        input_args.last().unwrap().ends_with("MOVIE.idx"),
        "{input_args:?}"
    );
}

/// The subtitle input reads like the media file (`-analyzeduration`,
/// `-probesize`), and a burned-in bitmap is decoded onto the canvas it was
/// authored for; the `-i` names it as a `file:` URL. With no `-isync` known it
/// is read from its start on its own timestamps, moved back by the media
/// file's seek (gl-929).
#[test]
fn a_burned_external_bitmap_is_input_1_on_its_canvas() {
    let mut sub = external_subtitle(2, "PGSSUB", "/media/movie en.sup");
    sub.width = Some(1920);
    sub.height = Some(1080);
    let mut state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Encode), None);
    state.media_source.analyze_duration_ms = Some(3000);
    assert_eq!(
        external_stream_input_arguments(
            &state,
            Some("00:01:02.500"),
            None,
            &InputAnalysis::default()
        ),
        [
            "-analyzeduration",
            "3000000",
            "-probesize",
            "1G",
            "-canvas_size",
            "1920x1080",
            "-seek_timestamp",
            "1",
            "-itsoffset",
            "-00:01:02.500",
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
            Some(FfmpegVersion::new(7, 0)),
            &InputAnalysis::default()
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

/// The embedded half of that pair: where `-isync` exists, an embedded bitmap
/// sidecar is read from its start on the media file's clock too, with no
/// canvas, whether the media file seeks or not (gl-1200). Seeked like the
/// media file, its seek landed past the start by its first cue's time and
/// inside a display set: an embed from 1, 3 or 9 s kept only the end segment
/// of the first display set after the start, so that subtitle never showed.
/// The cues before the start are cut on the output side (gl-930).
#[rstest]
#[case::seeked(Some("10"))]
#[case::from_the_start(None)]
#[case::seek_clamped_to_the_start(Some("00:00:00.000"))]
fn an_embedded_external_bitmap_is_read_from_its_start_and_synced(#[case] seek: Option<&str>) {
    let sub = external_subtitle(2, "PGSSUB", "/media/movie.en.sup");
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    assert_eq!(
        external_stream_input_arguments(
            &state,
            seek,
            Some(FfmpegVersion::new(7, 0)),
            &InputAnalysis::default()
        ),
        [
            "-analyzeduration",
            "200M",
            "-probesize",
            "1G",
            "-isync",
            "0",
            "-i",
            "file:/media/movie.en.sup",
        ]
    );
}

/// A VobSub sidecar is a bitmap too: embedded where `-isync` exists, it is
/// read through its `.idx` from its start, on the media file's clock.
#[test]
fn an_embedded_vob_sub_is_read_from_its_start_and_synced() {
    let dir = tempfile::tempdir().unwrap();
    let sub_file = dir.path().join("movie.sub");
    std::fs::write(&sub_file, "dummy").unwrap();
    std::fs::write(dir.path().join("movie.idx"), "dummy").unwrap();
    let sub = external_subtitle(2, "dvdsub", &sub_file.to_string_lossy());
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    let input_args = external_stream_input_arguments(
        &state,
        Some("10"),
        Some(FfmpegVersion::new(5, 1)),
        &InputAnalysis::default(),
    );
    assert_eq!(
        input_args[4..6],
        ["-isync", "0"],
        "no seek before the sync: {input_args:?}"
    );
    assert!(input_args.last().unwrap().ends_with("movie.idx"));
}

/// An embedded external track is input 1 too, with no canvas: it is copied
/// or converted, not drawn.
#[test]
fn an_embedded_external_text_track_is_input_1_without_a_canvas() {
    let sub = external_subtitle(2, "srt", "/media/movie.en.srt");
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    assert_eq!(
        external_stream_input_arguments(&state, None, None, &InputAnalysis::default()),
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

/// A sidecar whose path is a URL is read from the URL, embedded or burned in:
/// a `file:` prefix would have ffmpeg open a local file named after it
/// (gl-1722, a Jellyfin bug).
#[rstest]
#[case(
    "srt",
    SubtitleDeliveryMethod::Embed,
    "http://relay.example/remote.en.srt"
)]
#[case(
    "PGSSUB",
    SubtitleDeliveryMethod::Encode,
    "https://relay.example/remote en.sup"
)]
fn a_url_sidecar_is_input_1_as_its_url(
    #[case] codec: &str,
    #[case] method: SubtitleDeliveryMethod,
    #[case] url: &str,
) {
    let sub = external_subtitle(2, codec, url);
    let state = build_state(Some(sub), Some(method), None);
    let input_args = external_stream_input_arguments(&state, None, None, &InputAnalysis::default());
    assert_eq!(input_args[input_args.len() - 2..], ["-i", url]);
}

/// A configured `FFmpeg:analyzeduration` / `FFmpeg:probesize` reaches the
/// subtitle input as it reaches the media file (`GetInputArgument`'s
/// `NeedsExternalSubtitleMuxing` branch), and a source's own analyze
/// duration still wins.
#[test]
fn an_external_subtitle_input_reads_with_the_configured_analysis() {
    let configured = InputAnalysis {
        analyze_duration: "8M".parse().unwrap(),
        probe_size: "32M".parse().unwrap(),
    };
    let sub = external_subtitle(2, "srt", "/media/movie.en.srt");
    let mut state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    let flags = |state: &EncodingJobInfo| {
        external_stream_input_arguments(state, None, None, &configured)[..4].to_vec()
    };
    assert_eq!(
        flags(&state),
        ["-analyzeduration", "8M", "-probesize", "32M"]
    );
    state.media_source.analyze_duration_ms = Some(3000);
    assert_eq!(
        flags(&state),
        ["-analyzeduration", "3000000", "-probesize", "32M"]
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
    let found = external_stream_input_arguments(
        &state,
        Some("10"),
        Some(FfmpegVersion::new(7, 0)),
        &InputAnalysis::default(),
    );
    assert!(found.is_empty(), "{found:?}");
}

/// Without `-copyts` every input's timeline starts at its own first packet,
/// so each external subtitle input is put on the media file's clock by the
/// `-isync` ffmpeg 5.1 brought; a bitmap sidecar, whose start time is its
/// first cue, is not seeked there (gl-1200). On an older or unknown ffmpeg a
/// bitmap sidecar is read from its start on its own timestamps
/// (`-seek_timestamp`) and moved back by the seek (gl-929); a text sidecar has
/// no start time and keeps upstream's seeked form.
#[rstest]
#[case::with_isync(Some(FfmpegVersion::new(5, 1)), "srt", &["-ss", "10", "-isync", "0"])]
#[case::newer(Some(FfmpegVersion::with_build(7, 0, 1)), "srt", &["-ss", "10", "-isync", "0"])]
#[case::bitmap_with_isync(Some(FfmpegVersion::new(5, 1)), "PGSSUB", &["-isync", "0"])]
#[case::text_before_isync(Some(FfmpegVersion::new(5, 0)), "srt", &["-ss", "10"])]
#[case::text_unknown(None, "srt", &["-ss", "10"])]
#[case::bitmap_before_isync(
    Some(FfmpegVersion::new(5, 0)),
    "PGSSUB",
    &["-seek_timestamp", "1", "-itsoffset", "-10"]
)]
#[case::bitmap_oldest(
    Some(FfmpegVersion::new(4, 4)),
    "PGSSUB",
    &["-seek_timestamp", "1", "-itsoffset", "-10"]
)]
#[case::bitmap_unknown(None, "PGSSUB", &["-seek_timestamp", "1", "-itsoffset", "-10"])]
fn external_inputs_are_synced_to_the_media_files_clock(
    #[case] version: Option<FfmpegVersion>,
    #[case] codec: &str,
    #[case] timing: &[&str],
) {
    let path = if codec == "srt" {
        "/media/movie.en.srt"
    } else {
        "/media/movie.en.sup"
    };
    let sub = external_subtitle(2, codec, path);
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    let inputs =
        external_stream_input_arguments(&state, Some("10"), version, &InputAnalysis::default());
    let mut expected = vec!["-analyzeduration", "200M", "-probesize", "1G"];
    expected.extend(timing);
    let input = format!("file:{path}");
    expected.extend(["-i", input.as_str()]);
    assert_eq!(inputs, expected);
}

/// The copy of a text sidecar the server cut to a seeked embed's start
/// (gl-1199) is read in the sidecar's place, neither seeked nor synced: its
/// cues already start at the start. An external audio track after it is still
/// seeked like the media file.
#[rstest]
#[case::before_5_0(Some(FfmpegVersion::new(4, 4)))]
#[case::unknown_version(None)]
#[case::with_isync(Some(FfmpegVersion::new(5, 1)))]
fn a_trimmed_copy_of_a_text_sidecar_is_read_unseeked(#[case] version: Option<FfmpegVersion>) {
    let audio = MediaStream {
        is_external: true,
        path: Some("/media/movie.commentary.mka".to_owned()),
        ..stream(3, MediaStreamType::Audio, "aac")
    };
    let sub = external_subtitle(2, "srt", "/media/movie.en.srt");
    let mut state = build_state(
        Some(sub.clone()),
        Some(SubtitleDeliveryMethod::Embed),
        Some(vec![audio.clone(), sub]),
    );
    state.audio_stream = Some(audio);
    state.trimmed_subtitle_path = Some("/cache/transcodes/abc-stream.90000000.srt".into());

    let inputs =
        external_stream_input_arguments(&state, Some("9"), version, &InputAnalysis::default());

    let sync: &[&str] = if version.is_some_and(|v| v >= FfmpegVersion::new(5, 1)) {
        &["-isync", "0"]
    } else {
        &[]
    };
    let mut expected = vec![
        "-analyzeduration",
        "200M",
        "-probesize",
        "1G",
        "-i",
        "file:/cache/transcodes/abc-stream.90000000.srt",
        "-ss",
        "9",
    ];
    expected.extend(sync);
    expected.extend(["-i", "/media/movie.commentary.mka"]);
    assert_eq!(inputs, expected);
}

/// The copy holds the one track, so it is mapped as the first stream of
/// input 1 even when it came out of a Matroska subtitle file's second.
#[test]
fn a_trimmed_copy_of_an_mks_track_maps_its_only_stream() {
    let mks0 = external_subtitle(2, "subrip", "/media/movie.mks");
    let mks1 = external_subtitle(3, "ass", "/media/movie.mks");
    let mut state = build_state(
        Some(mks1.clone()),
        Some(SubtitleDeliveryMethod::Embed),
        Some(vec![mks0, mks1]),
    );
    assert!(
        map_args(&state).contains("-map 1:1"),
        "{}",
        map_args(&state)
    );
    state.trimmed_subtitle_path = Some("/cache/transcodes/abc-stream.90000000.ass".into());
    let args = map_args(&state);
    assert!(args.ends_with(" -map 1:0"), "{args}");
}

/// A seek clamped to the very start still names its time, which offsets a
/// bitmap read from its start by nothing.
#[test]
fn a_seek_clamped_to_the_start_offsets_a_bitmap_by_nothing() {
    let sub = external_subtitle(2, "PGSSUB", "/media/movie.en.sup");
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Embed), None);
    assert_eq!(
        external_stream_input_arguments(
            &state,
            Some("00:00:00.000"),
            None,
            &InputAnalysis::default()
        ),
        [
            "-analyzeduration",
            "200M",
            "-probesize",
            "1G",
            "-seek_timestamp",
            "1",
            "-itsoffset",
            "-00:00:00.000",
            "-i",
            "file:/media/movie.en.sup",
        ]
    );
}

/// Without `-isync`, a burned-in bitmap sidecar is read from its start too,
/// after its canvas: a seek before its first cue would lose that display set.
/// With no seek it takes `-seek_timestamp` alone, so its timeline is not moved
/// by its first cue (gl-929).
#[rstest]
#[case::seeked(Some("10"), &["-canvas_size", "1920x1080", "-seek_timestamp", "1", "-itsoffset", "-10"])]
#[case::from_the_start(None, &["-canvas_size", "1920x1080", "-seek_timestamp", "1"])]
fn without_isync_a_burned_bitmap_is_read_from_its_start(
    #[case] seek: Option<&str>,
    #[case] timing: &[&str],
) {
    let mut sub = external_subtitle(2, "PGSSUB", "/media/movie.en.sup");
    sub.width = Some(1920);
    sub.height = Some(1080);
    let state = build_state(Some(sub), Some(SubtitleDeliveryMethod::Encode), None);
    let mut expected = vec!["-analyzeduration", "200M", "-probesize", "1G"];
    expected.extend(timing);
    expected.extend(["-i", "file:/media/movie.en.sup"]);
    assert_eq!(
        external_stream_input_arguments(
            &state,
            seek,
            Some(FfmpegVersion::new(5, 0)),
            &InputAnalysis::default()
        ),
        expected
    );
}

/// Without `-isync`, an external audio track keeps upstream's form: seeked,
/// with no option for its clock.
#[rstest]
#[case::before_isync(Some(FfmpegVersion::new(5, 0)))]
#[case::unknown(None)]
fn without_isync_an_external_audio_track_is_seeked_as_upstream(
    #[case] version: Option<FfmpegVersion>,
) {
    let audio = MediaStream {
        is_external: true,
        path: Some("/media/movie.commentary.mka".to_owned()),
        ..stream(3, MediaStreamType::Audio, "aac")
    };
    let mut state = build_state(None, None, Some(vec![audio.clone()]));
    state.audio_stream = Some(audio);
    assert_eq!(
        external_stream_input_arguments(&state, Some("10"), version, &InputAnalysis::default()),
        ["-ss", "10", "-i", "/media/movie.commentary.mka"]
    );
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

    let inputs = external_stream_input_arguments(
        &state,
        Some("10"),
        Some(FfmpegVersion::new(7, 0)),
        &InputAnalysis::default(),
    );
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
