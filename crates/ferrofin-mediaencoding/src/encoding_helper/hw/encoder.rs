//! Which encoder ffmpeg should use — the hardware side of the choice.
//!
//! Port of C# `EncodingHelper.GetVideoEncoder` and the `GetH26xOrAv1Encoder` /
//! `GetMjpegEncoder` helpers it dispatches to (10.11.z lines 194–260, 464–504).
//!
//! The rule is uniform: for a plain video file, if the operator selected a
//! hardware type, left hardware encoding enabled, and the running ffmpeg
//! actually carries `<codec>_<vendor>`, use it; otherwise fall back to the
//! software encoder. Folder rips (ISO/DVD/Blu-ray) always use software, because
//! upstream found that a failed hardware session inside a `concat` input leaves
//! ffmpeg retrying forever instead of exiting.

use ferrofin_model::entities::{HardwareAccelerationType, VideoType};

use super::capabilities::FfmpegCapabilities;
use crate::encoding_helper::helper::is_valid_container;
use crate::encoding_helper::transcode_state::EncoderCapabilities as _;

/// The software MJPEG encoder, and the fallback for every MJPEG case. Port of
/// C# `_defaultMjpegEncoder`.
pub const DEFAULT_MJPEG_ENCODER: &str = "mjpeg";

/// The vendor suffix for a hardware accelerator, or `None` where that
/// accelerator has no encoder of its own.
///
/// Port of the `codecMap` built inside `GetH26xOrAv1Encoder`. `none` is not a
/// vendor, so it maps to `None` and the software encoder wins.
fn hw_encoder_suffix(hw_type: HardwareAccelerationType) -> Option<&'static str> {
    match hw_type {
        HardwareAccelerationType::amf => Some("amf"),
        HardwareAccelerationType::nvenc => Some("nvenc"),
        HardwareAccelerationType::qsv => Some("qsv"),
        HardwareAccelerationType::vaapi => Some("vaapi"),
        HardwareAccelerationType::videotoolbox => Some("videotoolbox"),
        HardwareAccelerationType::v4l2m2m => Some("v4l2m2m"),
        HardwareAccelerationType::rkmpp => Some("rkmpp"),
        HardwareAccelerationType::none => None,
    }
}

/// The vendor suffix for a hardware **MJPEG** encoder. Port of C#
/// `_mjpegCodecMap`, which is deliberately shorter than the H.26x map:
/// NVENC and AMF have no MJPEG encoder at all.
fn mjpeg_encoder_suffix(hw_type: HardwareAccelerationType) -> Option<&'static str> {
    match hw_type {
        HardwareAccelerationType::vaapi => Some("vaapi"),
        HardwareAccelerationType::qsv => Some("qsv"),
        HardwareAccelerationType::videotoolbox => Some("videotoolbox"),
        HardwareAccelerationType::rkmpp => Some("rkmpp"),
        // Listed rather than wildcarded so a new accelerator forces an author
        // to decide, the same way the H.26x map does.
        HardwareAccelerationType::amf
        | HardwareAccelerationType::nvenc
        | HardwareAccelerationType::v4l2m2m
        | HardwareAccelerationType::none => None,
    }
}

/// Whether a hardware encoder may be considered at all for this job.
///
/// Port of the `state.VideoType == VideoType.VideoFile` guard. A `None` video
/// type is treated as a plain video file, which is what an ordinary library
/// item is and what C# holds in its non-nullable default.
fn hardware_encoding_allowed(
    video_type: Option<VideoType>,
    enable_hardware_encoding: bool,
) -> bool {
    matches!(
        video_type.unwrap_or(VideoType::VideoFile),
        VideoType::VideoFile
    ) && enable_hardware_encoding
}

/// Picks the H.264 / HEVC / AV1 encoder. Port of `GetH26xOrAv1Encoder`.
///
/// `default_encoder` is the software fallback (`libx264` / `libx265` /
/// `libsvtav1`) and `hw_encoder` the codec half of the hardware name (`h264` /
/// `hevc` / `av1`).
#[must_use]
pub fn h26x_or_av1_encoder(
    default_encoder: &'static str,
    hw_encoder: &str,
    caps: &FfmpegCapabilities,
    hw_type: HardwareAccelerationType,
    video_type: Option<VideoType>,
    enable_hardware_encoding: bool,
) -> String {
    if hardware_encoding_allowed(video_type, enable_hardware_encoding)
        && let Some(suffix) = hw_encoder_suffix(hw_type)
    {
        let preferred = format!("{hw_encoder}_{suffix}");
        if caps.supports_encoder(&preferred) {
            return preferred;
        }
    }
    default_encoder.to_owned()
}

/// Picks the MJPEG encoder. Port of `GetMjpegEncoder`.
///
/// VAAPI is special-cased: the legacy i965 driver has no MJPEG encoder at all,
/// so a VAAPI device that is not Intel iHD falls straight back to software
/// even if `mjpeg_vaapi` is listed.
#[must_use]
pub fn mjpeg_encoder(
    caps: &FfmpegCapabilities,
    hw_type: HardwareAccelerationType,
    video_type: Option<VideoType>,
    enable_hardware_encoding: bool,
) -> String {
    if matches!(
        video_type.unwrap_or(VideoType::VideoFile),
        VideoType::VideoFile
    ) {
        if hw_type == HardwareAccelerationType::vaapi && !caps.is_vaapi_device_intel_ihd() {
            return DEFAULT_MJPEG_ENCODER.to_owned();
        }
        if enable_hardware_encoding && let Some(suffix) = mjpeg_encoder_suffix(hw_type) {
            let preferred = format!("{DEFAULT_MJPEG_ENCODER}_{suffix}");
            if caps.supports_encoder(&preferred) {
                return preferred;
            }
        }
    }
    DEFAULT_MJPEG_ENCODER.to_owned()
}

/// Picks the video encoder for an output codec. Port of `GetVideoEncoder`.
///
/// A `None` or empty output codec, or one that is not a codec name we build an
/// encoder for, yields `"copy"` — the stream-copy path, where no encoder runs.
/// An unrecognised codec is passed through lowercased only if it satisfies
/// upstream's `ContainerValidationRegexStr` character allowlist, since the
/// result lands directly on an ffmpeg command line.
#[must_use]
pub fn video_encoder(
    output_video_codec: Option<&str>,
    caps: &FfmpegCapabilities,
    hw_type: HardwareAccelerationType,
    video_type: Option<VideoType>,
    enable_hardware_encoding: bool,
) -> String {
    let Some(codec) = output_video_codec.filter(|c| !c.is_empty()) else {
        return "copy".to_owned();
    };
    let lower = codec.to_ascii_lowercase();
    match lower.as_str() {
        "av1" => h26x_or_av1_encoder(
            "libsvtav1",
            "av1",
            caps,
            hw_type,
            video_type,
            enable_hardware_encoding,
        ),
        "h265" | "hevc" => h26x_or_av1_encoder(
            "libx265",
            "hevc",
            caps,
            hw_type,
            video_type,
            enable_hardware_encoding,
        ),
        "h264" => h26x_or_av1_encoder(
            "libx264",
            "h264",
            caps,
            hw_type,
            video_type,
            enable_hardware_encoding,
        ),
        "mjpeg" => mjpeg_encoder(caps, hw_type, video_type, enable_hardware_encoding),
        _ if is_valid_container(&lower) => lower,
        _ => "copy".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // Hand-derived from the C# at EncodingHelper.cs 10.11.z lines 194-260 and
    // 464-504; upstream ships no tests for these.

    /// A capability set carrying every hardware encoder Jellyfin knows about.
    fn all_hw_encoders() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .encoders(crate::encoder::REQUIRED_ENCODERS)
            .build()
    }

    #[rstest]
    #[case(HardwareAccelerationType::amf, "h264_amf")]
    #[case(HardwareAccelerationType::nvenc, "h264_nvenc")]
    #[case(HardwareAccelerationType::qsv, "h264_qsv")]
    #[case(HardwareAccelerationType::vaapi, "h264_vaapi")]
    #[case(HardwareAccelerationType::videotoolbox, "h264_videotoolbox")]
    #[case(HardwareAccelerationType::v4l2m2m, "h264_v4l2m2m")]
    #[case(HardwareAccelerationType::rkmpp, "h264_rkmpp")]
    #[case(HardwareAccelerationType::none, "libx264")]
    fn every_accelerator_maps_to_its_h264_encoder(
        #[case] hw_type: HardwareAccelerationType,
        #[case] expected: &str,
    ) {
        assert_eq!(
            video_encoder(Some("h264"), &all_hw_encoders(), hw_type, None, true),
            expected
        );
    }

    #[rstest]
    // HEVC and AV1 follow the same map; `h265` is an accepted spelling of hevc.
    #[case("hevc", HardwareAccelerationType::nvenc, "hevc_nvenc")]
    #[case("h265", HardwareAccelerationType::nvenc, "hevc_nvenc")]
    #[case("hevc", HardwareAccelerationType::none, "libx265")]
    #[case("av1", HardwareAccelerationType::qsv, "av1_qsv")]
    #[case("av1", HardwareAccelerationType::none, "libsvtav1")]
    // AV1 has no VideoToolbox or RKMPP encoder, so those fall back to software.
    #[case("av1", HardwareAccelerationType::videotoolbox, "libsvtav1")]
    #[case("av1", HardwareAccelerationType::rkmpp, "libsvtav1")]
    fn hevc_and_av1_follow_the_same_map(
        #[case] codec: &str,
        #[case] hw_type: HardwareAccelerationType,
        #[case] expected: &str,
    ) {
        assert_eq!(
            video_encoder(Some(codec), &all_hw_encoders(), hw_type, None, true),
            expected
        );
    }

    #[test]
    fn an_encoder_the_build_lacks_falls_back_to_software() {
        // The operator asked for NVENC, but this ffmpeg has no h264_nvenc.
        let caps = FfmpegCapabilities::builder()
            .encoders(["libx264", "libx265", "libsvtav1"])
            .build();
        assert_eq!(
            video_encoder(
                Some("h264"),
                &caps,
                HardwareAccelerationType::nvenc,
                None,
                true
            ),
            "libx264"
        );
    }

    #[test]
    fn disabling_hardware_encoding_forces_software() {
        assert_eq!(
            video_encoder(
                Some("h264"),
                &all_hw_encoders(),
                HardwareAccelerationType::nvenc,
                None,
                false
            ),
            "libx264"
        );
    }

    #[rstest]
    #[case(VideoType::Iso)]
    #[case(VideoType::Dvd)]
    #[case(VideoType::BluRay)]
    fn folder_rips_never_use_a_hardware_encoder(#[case] video_type: VideoType) {
        // Upstream's reason: a failed hardware session inside a `concat` input
        // leaves ffmpeg retrying forever rather than exiting.
        assert_eq!(
            video_encoder(
                Some("h264"),
                &all_hw_encoders(),
                HardwareAccelerationType::nvenc,
                Some(video_type),
                true
            ),
            "libx264"
        );
    }

    #[test]
    fn a_plain_video_file_is_the_default_reading_of_an_absent_video_type() {
        for video_type in [None, Some(VideoType::VideoFile)] {
            assert_eq!(
                video_encoder(
                    Some("h264"),
                    &all_hw_encoders(),
                    HardwareAccelerationType::nvenc,
                    video_type,
                    true
                ),
                "h264_nvenc"
            );
        }
    }

    #[rstest]
    #[case(HardwareAccelerationType::qsv, "mjpeg_qsv")]
    #[case(HardwareAccelerationType::videotoolbox, "mjpeg_videotoolbox")]
    #[case(HardwareAccelerationType::rkmpp, "mjpeg_rkmpp")]
    // NVENC, AMF and V4L2 have no MJPEG encoder: software.
    #[case(HardwareAccelerationType::nvenc, "mjpeg")]
    #[case(HardwareAccelerationType::amf, "mjpeg")]
    #[case(HardwareAccelerationType::v4l2m2m, "mjpeg")]
    #[case(HardwareAccelerationType::none, "mjpeg")]
    fn mjpeg_has_its_own_shorter_map(
        #[case] hw_type: HardwareAccelerationType,
        #[case] expected: &str,
    ) {
        assert_eq!(
            video_encoder(Some("mjpeg"), &all_hw_encoders(), hw_type, None, true),
            expected
        );
    }

    #[test]
    fn vaapi_mjpeg_needs_the_intel_ihd_driver() {
        // i965 and unknown-driver devices have no MJPEG encoder at all, so the
        // driver check comes BEFORE the capability check and wins.
        let i965 = FfmpegCapabilities::builder()
            .encoders(crate::encoder::REQUIRED_ENCODERS)
            .vaapi_driver(false, false, true)
            .build();
        assert_eq!(
            mjpeg_encoder(&i965, HardwareAccelerationType::vaapi, None, true),
            "mjpeg"
        );

        let ihd = FfmpegCapabilities::builder()
            .encoders(crate::encoder::REQUIRED_ENCODERS)
            .vaapi_driver(false, true, false)
            .build();
        assert_eq!(
            mjpeg_encoder(&ihd, HardwareAccelerationType::vaapi, None, true),
            "mjpeg_vaapi"
        );
    }

    #[test]
    fn an_absent_or_empty_output_codec_is_a_stream_copy() {
        assert_eq!(
            video_encoder(
                None,
                &all_hw_encoders(),
                HardwareAccelerationType::nvenc,
                None,
                true
            ),
            "copy"
        );
        assert_eq!(
            video_encoder(
                Some(""),
                &all_hw_encoders(),
                HardwareAccelerationType::nvenc,
                None,
                true
            ),
            "copy"
        );
    }

    #[rstest]
    // A codec we build no encoder for passes through lowercased, provided it
    // satisfies upstream's character allowlist.
    #[case("VP9", "vp9")]
    #[case("theora", "theora")]
    #[case("prores_ks", "prores_ks")]
    // ...and becomes a stream copy when it does not. These are the shapes that
    // matter: anything reaching here goes onto an ffmpeg command line.
    #[case("../evil", "copy")]
    #[case("h264 -f lavfi", "copy")]
    #[case("codec;rm -rf", "copy")]
    #[case("a$(whoami)", "copy")]
    // 40 characters is the allowlist's limit; 41 is not.
    #[case(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    )]
    #[case("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "copy")]
    fn an_unrecognised_codec_is_passed_through_only_when_it_validates(
        #[case] codec: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(
            video_encoder(
                Some(codec),
                &all_hw_encoders(),
                HardwareAccelerationType::none,
                None,
                true
            ),
            expected
        );
    }

    #[test]
    fn codec_names_are_matched_case_insensitively() {
        assert_eq!(
            video_encoder(
                Some("H264"),
                &all_hw_encoders(),
                HardwareAccelerationType::nvenc,
                None,
                true
            ),
            "h264_nvenc"
        );
    }
}
