//! Port of `MediaBrowser.Model.Dlna.ConditionProcessor`.
//!
//! Evaluates a single [`ProfileCondition`] against a concrete stream value,
//! deciding whether the condition is satisfied. The `StreamBuilder` uses the
//! set of *failed* conditions to accumulate transcode reasons.

use super::enums::{ProfileConditionType, ProfileConditionValue};
use super::profile_condition::ProfileCondition;
use crate::data::VideoRangeType;
use crate::media_info::TransportStreamTimestamp;

/// Parses a [`VideoRangeType`] from its wire name, case-insensitively.
///
/// Public wrapper over [`parse_video_range_type`] for reuse by `StreamInfo`.
#[must_use]
pub fn parse_video_range_type_pub(value: &str) -> Option<VideoRangeType> {
    parse_video_range_type(value)
}

/// Parses a [`VideoRangeType`] from its wire name, case-insensitively.
///
/// Mirrors C# `Enum.TryParse<VideoRangeType>(value, true, out _)`.
fn parse_video_range_type(value: &str) -> Option<VideoRangeType> {
    for candidate in video_range_type_names() {
        if candidate.1.eq_ignore_ascii_case(value) {
            return Some(candidate.0);
        }
    }
    None
}

/// The wire name of a [`VideoRangeType`], mirroring `Enum.GetName`.
pub(crate) fn video_range_type_name(value: VideoRangeType) -> &'static str {
    for candidate in video_range_type_names() {
        if candidate.0 == value {
            return candidate.1;
        }
    }
    "Unknown"
}

/// All [`VideoRangeType`] variants and their wire names, in declaration order
/// (mirrors `Enum.GetNames<VideoRangeType>()`).
fn video_range_type_names() -> [(VideoRangeType, &'static str); 13] {
    [
        (VideoRangeType::Unknown, "Unknown"),
        (VideoRangeType::Sdr, "SDR"),
        (VideoRangeType::Hdr10, "HDR10"),
        (VideoRangeType::Hlg, "HLG"),
        (VideoRangeType::Dovi, "DOVI"),
        (VideoRangeType::DoviWithHdr10, "DOVIWithHDR10"),
        (VideoRangeType::DoviWithHlg, "DOVIWithHLG"),
        (VideoRangeType::DoviWithSdr, "DOVIWithSDR"),
        (VideoRangeType::DoviWithEl, "DOVIWithEL"),
        (VideoRangeType::DoviWithHdr10Plus, "DOVIWithHDR10Plus"),
        (VideoRangeType::DoviWithElhdr10Plus, "DOVIWithELHDR10Plus"),
        (VideoRangeType::DoviInvalid, "DOVIInvalid"),
        (VideoRangeType::Hdr10Plus, "HDR10Plus"),
    ]
}

/// All [`VideoRangeType`] wire names (mirrors `Enum.GetNames<VideoRangeType>()`).
pub(crate) fn video_range_type_all_names() -> Vec<&'static str> {
    video_range_type_names()
        .iter()
        .map(|(_, name)| *name)
        .collect()
}

/// The condition processor.
pub struct ConditionProcessor;

impl ConditionProcessor {
    /// Checks if a video condition is satisfied.
    #[must_use]
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    pub fn is_video_condition_satisfied(
        condition: &ProfileCondition,
        width: Option<i32>,
        height: Option<i32>,
        video_bit_depth: Option<i32>,
        video_bitrate: Option<i32>,
        video_profile: Option<&str>,
        video_range_type: Option<VideoRangeType>,
        video_level: Option<f64>,
        video_framerate: Option<f32>,
        packet_length: Option<i32>,
        timestamp: Option<TransportStreamTimestamp>,
        is_anamorphic: Option<bool>,
        is_interlaced: Option<bool>,
        ref_frames: Option<i32>,
        num_streams: i32,
        num_video_streams: Option<i32>,
        num_audio_streams: Option<i32>,
        video_codec_tag: Option<&str>,
        is_avc: Option<bool>,
        video_rotation: Option<i32>,
    ) -> bool {
        match condition.property {
            ProfileConditionValue::IsInterlaced => is_bool_satisfied(condition, is_interlaced),
            ProfileConditionValue::IsAnamorphic => is_bool_satisfied(condition, is_anamorphic),
            ProfileConditionValue::IsAvc => is_bool_satisfied(condition, is_avc),
            ProfileConditionValue::VideoFramerate => {
                is_double_satisfied(condition, video_framerate.map(f64::from))
            }
            ProfileConditionValue::VideoLevel => is_double_satisfied(condition, video_level),
            ProfileConditionValue::VideoProfile => is_string_satisfied(condition, video_profile),
            ProfileConditionValue::VideoRangeType => {
                is_range_satisfied(condition, video_range_type)
            }
            ProfileConditionValue::VideoCodecTag => is_string_satisfied(condition, video_codec_tag),
            ProfileConditionValue::PacketLength => is_int_satisfied(condition, packet_length),
            ProfileConditionValue::VideoBitDepth => is_int_satisfied(condition, video_bit_depth),
            ProfileConditionValue::VideoBitrate => is_int_satisfied(condition, video_bitrate),
            ProfileConditionValue::Height => is_int_satisfied(condition, height),
            ProfileConditionValue::Width => is_int_satisfied(condition, width),
            ProfileConditionValue::RefFrames => is_int_satisfied(condition, ref_frames),
            ProfileConditionValue::NumStreams => is_int_satisfied(condition, Some(num_streams)),
            ProfileConditionValue::NumAudioStreams => {
                is_int_satisfied(condition, num_audio_streams)
            }
            ProfileConditionValue::NumVideoStreams => {
                is_int_satisfied(condition, num_video_streams)
            }
            ProfileConditionValue::VideoTimestamp => is_timestamp_satisfied(condition, timestamp),
            ProfileConditionValue::VideoRotation => is_int_satisfied(condition, video_rotation),
            _ => true,
        }
    }

    /// Checks if an image condition is satisfied.
    ///
    /// # Panics
    ///
    /// Panics if the condition's property is not `Height` or `Width`, mirroring
    /// the C# `ArgumentException`.
    #[must_use]
    pub fn is_image_condition_satisfied(
        condition: &ProfileCondition,
        width: Option<i32>,
        height: Option<i32>,
    ) -> bool {
        match condition.property {
            ProfileConditionValue::Height => is_int_satisfied(condition, height),
            ProfileConditionValue::Width => is_int_satisfied(condition, width),
            other => panic!("Unexpected condition on image file: {other:?}"),
        }
    }

    /// Checks if an audio condition is satisfied.
    ///
    /// # Panics
    ///
    /// Panics on an unexpected condition property, mirroring the C#
    /// `ArgumentException`.
    #[must_use]
    pub fn is_audio_condition_satisfied(
        condition: &ProfileCondition,
        audio_channels: Option<i32>,
        audio_bitrate: Option<i32>,
        audio_sample_rate: Option<i32>,
        audio_bit_depth: Option<i32>,
    ) -> bool {
        match condition.property {
            ProfileConditionValue::AudioBitrate => is_int_satisfied(condition, audio_bitrate),
            ProfileConditionValue::AudioChannels => is_int_satisfied(condition, audio_channels),
            ProfileConditionValue::AudioSampleRate => {
                is_int_satisfied(condition, audio_sample_rate)
            }
            ProfileConditionValue::AudioBitDepth => is_int_satisfied(condition, audio_bit_depth),
            other => panic!("Unexpected condition on audio file: {other:?}"),
        }
    }

    /// Checks if an audio condition is satisfied for a video.
    ///
    /// # Panics
    ///
    /// Panics on an unexpected condition property, mirroring the C#
    /// `ArgumentException`.
    #[must_use]
    pub fn is_video_audio_condition_satisfied(
        condition: &ProfileCondition,
        audio_channels: Option<i32>,
        audio_bitrate: Option<i32>,
        audio_sample_rate: Option<i32>,
        audio_bit_depth: Option<i32>,
        audio_profile: Option<&str>,
        is_secondary_track: Option<bool>,
    ) -> bool {
        match condition.property {
            ProfileConditionValue::AudioProfile => is_string_satisfied(condition, audio_profile),
            ProfileConditionValue::AudioBitrate => is_int_satisfied(condition, audio_bitrate),
            ProfileConditionValue::AudioChannels => is_int_satisfied(condition, audio_channels),
            ProfileConditionValue::IsSecondaryAudio => {
                is_bool_satisfied(condition, is_secondary_track)
            }
            ProfileConditionValue::AudioSampleRate => {
                is_int_satisfied(condition, audio_sample_rate)
            }
            ProfileConditionValue::AudioBitDepth => is_int_satisfied(condition, audio_bit_depth),
            other => panic!("Unexpected condition on audio file: {other:?}"),
        }
    }
}

fn is_int_satisfied(condition: &ProfileCondition, current_value: Option<i32>) -> bool {
    let Some(current_value) = current_value else {
        return !condition.is_required;
    };

    if condition.condition == ProfileConditionType::EqualsAny {
        for single in condition.value.split('|') {
            if let Ok(v) = single.parse::<i32>()
                && v == current_value
            {
                return true;
            }
        }
        return false;
    }

    if let Ok(expected) = condition.value.parse::<i32>() {
        return match condition.condition {
            ProfileConditionType::Equals => current_value == expected,
            ProfileConditionType::GreaterThanEqual => current_value >= expected,
            ProfileConditionType::LessThanEqual => current_value <= expected,
            ProfileConditionType::NotEquals => current_value != expected,
            ProfileConditionType::EqualsAny => unreachable!(),
        };
    }

    false
}

fn is_string_satisfied(condition: &ProfileCondition, current_value: Option<&str>) -> bool {
    let current_value = current_value.unwrap_or("");
    if current_value.is_empty() {
        return !condition.is_required;
    }

    let expected = condition.value.as_str();

    match condition.condition {
        ProfileConditionType::EqualsAny => expected
            .split('|')
            .any(|v| v.eq_ignore_ascii_case(current_value)),
        ProfileConditionType::Equals => current_value.eq_ignore_ascii_case(expected),
        ProfileConditionType::NotEquals => !current_value.eq_ignore_ascii_case(expected),
        _ => panic!("Unexpected ProfileConditionType: {:?}", condition.condition),
    }
}

fn is_bool_satisfied(condition: &ProfileCondition, current_value: Option<bool>) -> bool {
    let Some(current_value) = current_value else {
        return !condition.is_required;
    };

    if let Ok(expected) = condition.value.parse::<bool>() {
        return match condition.condition {
            ProfileConditionType::Equals => current_value == expected,
            ProfileConditionType::NotEquals => current_value != expected,
            _ => panic!("Unexpected ProfileConditionType: {:?}", condition.condition),
        };
    }

    false
}

// Faithful port of C# double comparisons (`double.Equals`), which are exact.
#[allow(clippy::float_cmp)]
fn is_double_satisfied(condition: &ProfileCondition, current_value: Option<f64>) -> bool {
    let Some(current_value) = current_value else {
        return !condition.is_required;
    };

    if condition.condition == ProfileConditionType::EqualsAny {
        for single in condition.value.split('|') {
            if single.trim().parse::<f64>() == Ok(current_value) {
                return true;
            }
        }
        return false;
    }

    if let Ok(expected) = condition.value.parse::<f64>() {
        return match condition.condition {
            ProfileConditionType::Equals => current_value == expected,
            ProfileConditionType::GreaterThanEqual => current_value >= expected,
            ProfileConditionType::LessThanEqual => current_value <= expected,
            ProfileConditionType::NotEquals => current_value != expected,
            ProfileConditionType::EqualsAny => unreachable!(),
        };
    }

    false
}

fn is_timestamp_satisfied(
    condition: &ProfileCondition,
    timestamp: Option<TransportStreamTimestamp>,
) -> bool {
    let Some(timestamp) = timestamp else {
        return !condition.is_required;
    };

    let expected = match condition.value.to_ascii_lowercase().as_str() {
        "none" => TransportStreamTimestamp::None,
        "zero" => TransportStreamTimestamp::Zero,
        "valid" => TransportStreamTimestamp::Valid,
        _ => return false,
    };

    match condition.condition {
        ProfileConditionType::Equals => timestamp == expected,
        ProfileConditionType::NotEquals => timestamp != expected,
        _ => panic!("Unexpected ProfileConditionType: {:?}", condition.condition),
    }
}

fn is_range_satisfied(condition: &ProfileCondition, current_value: Option<VideoRangeType>) -> bool {
    let Some(current_value) = current_value.filter(|v| *v != VideoRangeType::Unknown) else {
        return !condition.is_required;
    };

    // Special case: HDR10 also satisfies if the video is HDR10Plus
    if current_value == VideoRangeType::Hdr10Plus
        && is_range_satisfied_value(condition, VideoRangeType::Hdr10)
    {
        return true;
    }

    is_range_satisfied_value(condition, current_value)
}

fn is_range_satisfied_value(condition: &ProfileCondition, current_value: VideoRangeType) -> bool {
    if condition.condition == ProfileConditionType::EqualsAny {
        for single in condition.value.split('|') {
            if let Some(v) = parse_video_range_type(single)
                && v == current_value
            {
                return true;
            }
        }
        return false;
    }

    if let Some(expected) = parse_video_range_type(&condition.value) {
        return match condition.condition {
            ProfileConditionType::Equals => current_value == expected,
            ProfileConditionType::NotEquals => current_value != expected,
            _ => panic!("Unexpected ProfileConditionType: {:?}", condition.condition),
        };
    }

    false
}

pub(crate) use video_range_type_name as range_type_name;
