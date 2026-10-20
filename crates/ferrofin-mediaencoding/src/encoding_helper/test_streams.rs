//! Test fixtures: video streams whose metadata gives a chosen range type.

use ferrofin_model::data::VideoRangeType;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;

/// `stream` as a video (its type forced to `Video`) whose transfer, HDR10+
/// flag and, for Dolby Vision, (profile, compatibility id) give
/// `range_type`. Every row also carries BT.2020 colours, as an HDR base
/// layer would; they do not change a row's range type. The cached range
/// fields stay unset, so a check that reads them instead of deriving the
/// range fails its HDR cases.
pub(crate) fn with_range(stream: MediaStream, range_type: VideoRangeType) -> MediaStream {
    let (dovi, transfer, hdr10_plus) = match range_type {
        VideoRangeType::Sdr => (None, "bt709", false),
        VideoRangeType::Hdr10 => (None, "smpte2084", false),
        VideoRangeType::Hdr10Plus => (None, "smpte2084", true),
        VideoRangeType::Hlg => (None, "arib-std-b67", false),
        VideoRangeType::Dovi => (Some((5, 0)), "smpte2084", false),
        VideoRangeType::DoviWithHdr10 => (Some((8, 1)), "smpte2084", false),
        VideoRangeType::DoviWithHdr10Plus => (Some((8, 1)), "smpte2084", true),
        VideoRangeType::DoviWithEl => (Some((7, 6)), "smpte2084", false),
        VideoRangeType::DoviWithElhdr10Plus => (Some((7, 6)), "smpte2084", true),
        VideoRangeType::DoviWithHlg => (Some((8, 4)), "arib-std-b67", false),
        VideoRangeType::DoviWithSdr => (Some((8, 2)), "bt709", false),
        VideoRangeType::DoviInvalid => (Some((8, 6)), "smpte2084", false),
        other => panic!("no fixture for {other:?}"),
    };
    let s = MediaStream {
        stream_type: MediaStreamType::Video,
        color_space: Some("bt2020nc".to_owned()),
        color_primaries: Some("bt2020".to_owned()),
        color_transfer: Some(transfer.to_owned()),
        dv_profile: dovi.map(|(profile, _)| profile),
        dv_bl_signal_compatibility_id: dovi.map(|(_, id)| id),
        rpu_present_flag: dovi.map(|_| 1),
        bl_present_flag: dovi.map(|_| 1),
        el_present_flag: dovi.map(|(profile, _)| i32::from(profile == 7)),
        hdr10_plus_present_flag: Some(hdr10_plus),
        ..stream
    };
    assert_eq!(s.video_range_type(), range_type, "fixture");
    s
}
