//! Tests for the `dto-core` port unit (`MediaBrowser.Model.Dto`,
//! `.Session`, `.Providers`, `.Querying`, `.Search`).
//!
//! `MediaBrowser.Model` has no standalone xUnit tests for these DTOs upstream
//! (`MediaSourceInfo` is exercised only via `Jellyfin.Model.Tests.Dlna.
//! StreamBuilderTests`, which belongs to the DLNA port unit). These tests
//! therefore (a) transliterate the deterministic C# instance methods whose
//! behaviour is the oracle, and (b) lock the serde/OpenAPI wire contract
//! (PascalCase property names, `[JsonIgnore]` exclusions, renamed properties).

use hermit_model::dto::{ItemCounts, MediaSourceInfo, MediaSourceType};
use hermit_model::entities::{MediaStreamType, Video3DFormat};
use hermit_model::entities_media::MediaStream;
use hermit_model::media_info::MediaProtocol;

fn audio_stream(
    index: i32,
    is_default: bool,
    is_external: bool,
    bit_rate: Option<i32>,
) -> MediaStream {
    MediaStream {
        index,
        stream_type: MediaStreamType::Audio,
        is_default,
        is_external,
        bit_rate,
        ..MediaStream::default()
    }
}

fn video_stream(index: i32, bit_rate: Option<i32>) -> MediaStream {
    MediaStream {
        index,
        stream_type: MediaStreamType::Video,
        bit_rate,
        ..MediaStream::default()
    }
}

// --- ItemCounts.TotalItemCount (verbatim from the C# summation) ------------

#[test]
fn item_counts_total_excludes_item_count() {
    let counts = ItemCounts {
        movie_count: 1,
        series_count: 2,
        episode_count: 3,
        artist_count: 4,
        program_count: 5,
        trailer_count: 6,
        song_count: 7,
        album_count: 8,
        music_video_count: 9,
        box_set_count: 10,
        book_count: 11,
        // ItemCount is intentionally NOT part of the total.
        item_count: 1000,
    };
    // 1+2+3+4+5+6+7+8+9+10+11 = 66
    assert_eq!(counts.total_item_count(), 66);
}

// --- MediaSourceInfo constructor defaults (from the C# ctor) ---------------

#[test]
fn media_source_info_default_matches_csharp_ctor() {
    let info = MediaSourceInfo::default();
    assert!(info.supports_transcoding);
    assert!(info.supports_direct_stream);
    assert!(info.supports_direct_play);
    assert!(info.supports_probing);
    assert!(!info.use_most_compatible_transcoding_profile);
    assert_eq!(info.type_, MediaSourceType::Default);
    assert_eq!(info.protocol, MediaProtocol::File);
    assert!(info.media_streams.is_empty());
    assert!(info.formats.is_empty());
}

// --- MediaSourceInfo.VideoStream -------------------------------------------

#[test]
fn video_stream_returns_first_video() {
    let info = MediaSourceInfo {
        media_streams: vec![
            audio_stream(0, true, false, Some(128)),
            video_stream(1, Some(5000)),
        ],
        ..MediaSourceInfo::default()
    };
    assert_eq!(info.video_stream().map(|s| s.index), Some(1));
}

#[test]
fn video_stream_none_when_no_video() {
    let info = MediaSourceInfo {
        media_streams: vec![audio_stream(0, true, false, None)],
        ..MediaSourceInfo::default()
    };
    assert!(info.video_stream().is_none());
}

// --- MediaSourceInfo.GetDefaultAudioStream ---------------------------------

#[test]
fn default_audio_prefers_explicit_index() {
    let info = MediaSourceInfo {
        media_streams: vec![
            audio_stream(0, true, false, None),
            audio_stream(1, false, false, None),
        ],
        ..MediaSourceInfo::default()
    };
    // Explicit index 1 wins over the is_default stream at index 0.
    assert_eq!(
        info.get_default_audio_stream(Some(1)).map(|s| s.index),
        Some(1)
    );
}

#[test]
fn default_audio_ignores_negative_one_index() {
    let info = MediaSourceInfo {
        media_streams: vec![
            audio_stream(0, false, false, None),
            audio_stream(1, true, false, None),
        ],
        ..MediaSourceInfo::default()
    };
    // -1 means "no explicit preference"; fall back to the is_default stream.
    assert_eq!(
        info.get_default_audio_stream(Some(-1)).map(|s| s.index),
        Some(1)
    );
}

#[test]
fn default_audio_falls_back_to_first_audio() {
    let info = MediaSourceInfo {
        media_streams: vec![
            video_stream(0, None),
            audio_stream(1, false, false, None),
            audio_stream(2, false, false, None),
        ],
        ..MediaSourceInfo::default()
    };
    // No explicit index, none marked default -> first audio stream.
    assert_eq!(
        info.get_default_audio_stream(None).map(|s| s.index),
        Some(1)
    );
}

// --- MediaSourceInfo.GetMediaStream / GetStreamCount -----------------------

#[test]
fn get_media_stream_matches_type_and_index() {
    let info = MediaSourceInfo {
        media_streams: vec![audio_stream(0, false, false, None), video_stream(1, None)],
        ..MediaSourceInfo::default()
    };
    assert_eq!(
        info.get_media_stream(MediaStreamType::Video, 1)
            .map(|s| s.index),
        Some(1)
    );
    assert!(info.get_media_stream(MediaStreamType::Video, 9).is_none());
}

#[test]
fn get_stream_count_none_when_empty() {
    let info = MediaSourceInfo::default();
    assert_eq!(info.get_stream_count(MediaStreamType::Audio), None);
}

#[test]
fn get_stream_count_counts_matches() {
    let info = MediaSourceInfo {
        media_streams: vec![
            audio_stream(0, false, false, None),
            audio_stream(1, false, false, None),
            video_stream(2, None),
        ],
        ..MediaSourceInfo::default()
    };
    assert_eq!(info.get_stream_count(MediaStreamType::Audio), Some(2));
    assert_eq!(info.get_stream_count(MediaStreamType::Video), Some(1));
    assert_eq!(info.get_stream_count(MediaStreamType::Subtitle), Some(0));
}

// --- MediaSourceInfo.IsSecondaryAudio --------------------------------------

#[test]
fn is_secondary_audio_external_is_false() {
    let info = MediaSourceInfo::default();
    let ext = audio_stream(3, false, true, None);
    assert_eq!(info.is_secondary_audio(&ext), Some(false));
}

#[test]
fn is_secondary_audio_first_internal_is_primary() {
    let info = MediaSourceInfo {
        media_streams: vec![
            audio_stream(0, false, false, None),
            audio_stream(1, false, false, None),
        ],
        ..MediaSourceInfo::default()
    };
    // The first internal audio track is primary (not secondary).
    assert_eq!(info.is_secondary_audio(&info.media_streams[0]), Some(false));
    // A later internal audio track is secondary.
    assert_eq!(info.is_secondary_audio(&info.media_streams[1]), Some(true));
}

// --- MediaSourceInfo.InferTotalBitrate -------------------------------------

#[test]
fn infer_total_bitrate_sums_internal_streams() {
    let mut info = MediaSourceInfo {
        media_streams: vec![
            video_stream(0, Some(5000)),
            audio_stream(1, false, false, Some(128)),
            audio_stream(2, false, true, Some(999)), // external -> excluded
        ],
        ..MediaSourceInfo::default()
    };
    info.infer_total_bitrate(false);
    assert_eq!(info.bitrate, Some(5128));
}

#[test]
fn infer_total_bitrate_respects_existing_unless_forced() {
    let mut info = MediaSourceInfo {
        bitrate: Some(1),
        media_streams: vec![video_stream(0, Some(5000))],
        ..MediaSourceInfo::default()
    };
    info.infer_total_bitrate(false);
    assert_eq!(info.bitrate, Some(1)); // kept
    info.infer_total_bitrate(true);
    assert_eq!(info.bitrate, Some(5000)); // recomputed
}

// --- Serde / OpenAPI wire contract -----------------------------------------

#[test]
fn media_source_info_serializes_pascal_case_and_renames() {
    let info = MediaSourceInfo {
        id: Some("abc".to_owned()),
        e_tag: Some("etag-value".to_owned()),
        video3d_format: Some(Video3DFormat::HalfSideBySide),
        ..MediaSourceInfo::default()
    };

    let json = serde_json::to_value(&info).unwrap();
    let obj = json.as_object().unwrap();

    // PascalCase property names.
    assert!(obj.contains_key("Id"));
    assert!(obj.contains_key("Protocol"));
    assert!(obj.contains_key("SupportsTranscoding"));
    // Renamed properties from the OpenAPI contract.
    assert!(obj.contains_key("ETag"));
    assert!(obj.contains_key("Type"));
    assert!(obj.contains_key("Video3DFormat"));
    // `[JsonIgnore]` upstream -> never serialized.
    assert!(!obj.contains_key("TranscodeReasons"));
    assert!(!obj.contains_key("DefaultAudioIndexSource"));
}

#[test]
fn media_source_info_round_trips() {
    let info = MediaSourceInfo {
        id: Some("src-1".to_owned()),
        name: Some("Main".to_owned()),
        media_streams: vec![
            video_stream(0, Some(4000)),
            audio_stream(1, true, false, Some(256)),
        ],
        ..MediaSourceInfo::default()
    };

    let json = serde_json::to_string(&info).unwrap();
    let back: MediaSourceInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(info, back);
}
