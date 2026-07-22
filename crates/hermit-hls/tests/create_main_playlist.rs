//! Integration coverage for [`DynamicHlsPlaylistGenerator::create_main_playlist`].
//!
//! The parity oracle proper is the three timing helpers (unit-tested inline);
//! these tests exercise the `.m3u8` string builder around them, including both
//! the equal-length and keyframe-derived segment paths and the fMP4 branch. The
//! un-mockable process I/O stays behind the [`KeyframeExtractor`] trait, so a
//! fake drives the remuxing path.

use std::sync::Arc;

use hermit_hls::{
    CreateMainPlaylistRequest, DynamicHlsPlaylistGenerator, EncodingOptionsProvider, HlsError,
    KeyframeExtractor,
};
use hermit_keyframes::keyframe_data::KeyframeData;
use hermit_model::configuration::EncodingOptions;
use uuid::Uuid;

/// Fake extractor: yields preset keyframe data (or nothing) regardless of input.
/// Metadata-based so it survives the construction filter.
struct FakeExtractor(Option<KeyframeData>);

impl KeyframeExtractor for FakeExtractor {
    fn is_metadata_based(&self) -> bool {
        true
    }

    fn try_extract_keyframes(&self, _item_id: Uuid, _file_path: &str) -> Option<KeyframeData> {
        self.0.clone()
    }
}

/// Builds a config accessor exposing `extensions` as the on-demand-allowed set.
fn config_with_extensions(extensions: Vec<String>) -> impl EncodingOptionsProvider {
    move || EncodingOptions {
        allow_on_demand_metadata_based_keyframe_extraction_for_extensions: extensions.clone(),
        ..EncodingOptions::default()
    }
}

const TICKS_PER_MILLISECOND: i64 = 10_000;

fn ms_to_ticks(ms: i64) -> i64 {
    ms * TICKS_PER_MILLISECOND
}

#[test]
fn equal_length_ts_playlist_has_three_segments() {
    let generator = DynamicHlsPlaylistGenerator::new(
        config_with_extensions(Vec::new()),
        vec![Arc::new(FakeExtractor(None))],
    );
    let request = CreateMainPlaylistRequest::new(
        None,
        "/media/movie.mkv",
        6000,
        ms_to_ticks(13000),
        "ts",
        "hls/main/",
        "?apikey=abc",
        false,
    );

    let playlist = generator
        .create_main_playlist(&request)
        .expect("equal-length segments are valid");

    // Segments 6.0, 6.0, 1.0 → three #EXTINF lines, ceil(6.0) target duration.
    assert!(playlist.starts_with("#EXTM3U\n"));
    assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD\n"));
    assert!(playlist.contains("#EXT-X-VERSION:3\n"));
    assert!(playlist.contains("#EXT-X-TARGETDURATION:6\n"));
    assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
    assert_eq!(playlist.matches("#EXTINF:").count(), 3);
    assert!(playlist.contains("#EXTINF:6.000000, nodesc\n"));
    assert!(playlist.contains("#EXTINF:1.000000, nodesc\n"));
    // First segment URL: prefix + index 0 + ".ts" + query + runtime/length ticks.
    assert!(
        playlist.contains(
            "hls/main/0.ts?apikey=abc&runtimeTicks=0&actualSegmentLengthTicks=60000000\n"
        )
    );
    // Second segment starts at 6s worth of ticks.
    assert!(playlist.contains(
        "hls/main/1.ts?apikey=abc&runtimeTicks=60000000&actualSegmentLengthTicks=60000000\n"
    ));
    assert!(playlist.trim_end().ends_with("#EXT-X-ENDLIST"));
    // No fMP4 init header for a .ts playlist.
    assert!(!playlist.contains("#EXT-X-MAP"));
}

#[test]
fn fmp4_playlist_emits_map_header_and_version_7() {
    let generator = DynamicHlsPlaylistGenerator::new(
        config_with_extensions(Vec::new()),
        vec![Arc::new(FakeExtractor(None))],
    );
    let request = CreateMainPlaylistRequest::new(
        None,
        "/media/movie.mkv",
        6000,
        ms_to_ticks(6000),
        "mp4",
        "hls/main/",
        "?token=xyz",
        false,
    );

    let playlist = generator
        .create_main_playlist(&request)
        .expect("equal-length segments are valid");

    assert!(playlist.contains("#EXT-X-VERSION:7\n"));
    // fMP4 init segment header at index -1.
    assert!(playlist.contains(
        "#EXT-X-MAP:URI=\"hls/main/-1.mp4?token=xyz&runtimeTicks=0&actualSegmentLengthTicks=0\"\n"
    ));
    // Single 6s segment.
    assert_eq!(playlist.matches("#EXTINF:").count(), 1);
    assert!(playlist.contains("hls/main/0.mp4?token=xyz"));
}

#[test]
fn remuxing_video_uses_keyframe_segments() {
    // Allowed extension + remuxing video + a media source id → keyframe path.
    let keyframes = KeyframeData::new(
        ms_to_ticks(35000),
        vec![
            0,
            ms_to_ticks(10427),
            ms_to_ticks(20854),
            ms_to_ticks(31240),
        ],
    );
    let generator = DynamicHlsPlaylistGenerator::new(
        config_with_extensions(vec![".mkv".to_string()]),
        vec![Arc::new(FakeExtractor(Some(keyframes)))],
    );
    let request = CreateMainPlaylistRequest::new(
        Some(Uuid::from_u128(1)),
        "/media/movie.mkv",
        6000,
        ms_to_ticks(35000),
        "ts",
        "hls/main/",
        "?apikey=abc",
        true,
    );

    let playlist = generator
        .create_main_playlist(&request)
        .expect("keyframe segments are always valid");

    // ComputeSegments case 1 → 10.427, 10.427, 10.386, 3.760 (4 segments).
    assert_eq!(playlist.matches("#EXTINF:").count(), 4);
    assert!(playlist.contains("#EXTINF:10.427000, nodesc\n"));
    assert!(playlist.contains("#EXTINF:3.760000, nodesc\n"));
    // ceil(10.427) = 11.
    assert!(playlist.contains("#EXT-X-TARGETDURATION:11\n"));
}

#[test]
fn remuxing_video_disallowed_extension_falls_back_to_equal_length() {
    // Remuxing video but extension not allowed → equal-length path (extractor
    // never consulted).
    let generator = DynamicHlsPlaylistGenerator::new(
        config_with_extensions(vec![".mp4".to_string()]),
        vec![Arc::new(FakeExtractor(Some(KeyframeData::new(
            ms_to_ticks(1),
            vec![0],
        ))))],
    );
    let request = CreateMainPlaylistRequest::new(
        Some(Uuid::from_u128(2)),
        "/media/movie.mkv",
        6000,
        ms_to_ticks(6000),
        "ts",
        "hls/main/",
        "?x=1",
        true,
    );

    let playlist = generator.create_main_playlist(&request).unwrap();
    // Equal-length: single 6.0s segment.
    assert_eq!(playlist.matches("#EXTINF:").count(), 1);
    assert!(playlist.contains("#EXTINF:6.000000, nodesc\n"));
}

#[test]
fn zero_runtime_without_keyframes_is_invalid_operation() {
    let generator = DynamicHlsPlaylistGenerator::new(
        config_with_extensions(Vec::new()),
        vec![Arc::new(FakeExtractor(None))],
    );
    let request = CreateMainPlaylistRequest::new(
        None,
        "/media/movie.mkv",
        6000,
        0,
        "ts",
        "hls/main/",
        "?x=1",
        false,
    );

    let err = generator
        .create_main_playlist(&request)
        .expect_err("zero runtime must fail");
    assert_eq!(
        err,
        HlsError::InvalidOperation {
            desired_segment_length_ms: 6000,
            total_runtime_ticks: 0,
        }
    );
}
