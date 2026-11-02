//! Port of `MediaBrowser.Model.Dlna.ConditionProcessor`.
//!
//! Evaluates a single [`ProfileCondition`] against a concrete stream value,
//! deciding whether the condition is satisfied. The `StreamBuilder` uses the
//! set of *failed* conditions to accumulate transcode reasons.
//!
//! A condition upstream cannot evaluate throws, and so aborts the whole
//! negotiation; here it is a [`StreamBuilderError`], never a panic, since the
//! conditions come from the client's device profile.

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

/// What upstream's `ConditionProcessor` and `StreamBuilder` throw for a device
/// profile or media options they cannot evaluate.
///
/// Upstream's `ExceptionMiddleware` answers an `ArgumentException` with a 400
/// and an `InvalidOperationException` with a 500;
/// [`Self::is_invalid_argument`] tells the two apart.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamBuilderError {
    /// A condition on a property this kind of stream has none of
    /// (`ArgumentException`).
    #[error("Unexpected condition on {stream} file: {property:?}")]
    UnexpectedProperty {
        /// The kind of stream evaluated: `image` or `audio`.
        stream: &'static str,
        /// The condition's property.
        property: ProfileConditionValue,
    },
    /// A timestamp condition whose value names no `TransportStreamTimestamp`
    /// (`Enum.Parse`'s `ArgumentException`).
    #[error("Requested value '{0}' was not found.")]
    UnknownTimestamp(String),
    /// Media options missing what the negotiation needs
    /// (`ValidateMediaOptions`' `ArgumentException`).
    #[error("{0}")]
    InvalidOptions(&'static str),
    /// A comparison the value's type has none of
    /// (`InvalidOperationException`).
    #[error("Unexpected ProfileConditionType: {0:?}")]
    UnexpectedConditionType(ProfileConditionType),
}

impl StreamBuilderError {
    /// Whether upstream throws an `ArgumentException` here (a 400), rather
    /// than an `InvalidOperationException` (a 500).
    #[must_use]
    pub fn is_invalid_argument(&self) -> bool {
        !matches!(self, Self::UnexpectedConditionType(_))
    }
}

/// The condition processor.
pub struct ConditionProcessor;

impl ConditionProcessor {
    /// Checks if a video condition is satisfied.
    ///
    /// # Errors
    ///
    /// [`StreamBuilderError`] when the condition compares its value in a way
    /// the value's type has none of, or names no timestamp.
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
    ) -> Result<bool, StreamBuilderError> {
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
            _ => Ok(true),
        }
    }

    /// Checks if an image condition is satisfied.
    ///
    /// # Errors
    ///
    /// [`StreamBuilderError::UnexpectedProperty`] when the property is not
    /// `Height` or `Width` (the C# `ArgumentException`), or the comparison
    /// error of the value it tests.
    pub fn is_image_condition_satisfied(
        condition: &ProfileCondition,
        width: Option<i32>,
        height: Option<i32>,
    ) -> Result<bool, StreamBuilderError> {
        match condition.property {
            ProfileConditionValue::Height => is_int_satisfied(condition, height),
            ProfileConditionValue::Width => is_int_satisfied(condition, width),
            property => Err(StreamBuilderError::UnexpectedProperty {
                stream: "image",
                property,
            }),
        }
    }

    /// Checks if an audio condition is satisfied.
    ///
    /// # Errors
    ///
    /// [`StreamBuilderError::UnexpectedProperty`] on a property an audio file
    /// has none of (the C# `ArgumentException`), or the comparison error of
    /// the value it tests.
    pub fn is_audio_condition_satisfied(
        condition: &ProfileCondition,
        audio_channels: Option<i32>,
        audio_bitrate: Option<i32>,
        audio_sample_rate: Option<i32>,
        audio_bit_depth: Option<i32>,
    ) -> Result<bool, StreamBuilderError> {
        match condition.property {
            ProfileConditionValue::AudioBitrate => is_int_satisfied(condition, audio_bitrate),
            ProfileConditionValue::AudioChannels => is_int_satisfied(condition, audio_channels),
            ProfileConditionValue::AudioSampleRate => {
                is_int_satisfied(condition, audio_sample_rate)
            }
            ProfileConditionValue::AudioBitDepth => is_int_satisfied(condition, audio_bit_depth),
            property => Err(StreamBuilderError::UnexpectedProperty {
                stream: "audio",
                property,
            }),
        }
    }

    /// Checks if an audio condition is satisfied for a video.
    ///
    /// # Errors
    ///
    /// [`StreamBuilderError::UnexpectedProperty`] on a property an audio track
    /// has none of (the C# `ArgumentException`), or the comparison error of
    /// the value it tests.
    pub fn is_video_audio_condition_satisfied(
        condition: &ProfileCondition,
        audio_channels: Option<i32>,
        audio_bitrate: Option<i32>,
        audio_sample_rate: Option<i32>,
        audio_bit_depth: Option<i32>,
        audio_profile: Option<&str>,
        is_secondary_track: Option<bool>,
    ) -> Result<bool, StreamBuilderError> {
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
            property => Err(StreamBuilderError::UnexpectedProperty {
                stream: "audio",
                property,
            }),
        }
    }
}

/// The `InvalidOperationException` every comparison throws for a condition
/// type its value has none of.
fn unexpected(condition: &ProfileCondition) -> StreamBuilderError {
    StreamBuilderError::UnexpectedConditionType(condition.condition)
}

fn is_int_satisfied(
    condition: &ProfileCondition,
    current_value: Option<i32>,
) -> Result<bool, StreamBuilderError> {
    let Some(current_value) = current_value else {
        return Ok(!condition.is_required);
    };

    if condition.condition == ProfileConditionType::EqualsAny {
        for single in condition.value.split('|') {
            if let Ok(v) = single.parse::<i32>()
                && v == current_value
            {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    if let Ok(expected) = condition.value.parse::<i32>() {
        return match condition.condition {
            ProfileConditionType::Equals => Ok(current_value == expected),
            ProfileConditionType::GreaterThanEqual => Ok(current_value >= expected),
            ProfileConditionType::LessThanEqual => Ok(current_value <= expected),
            ProfileConditionType::NotEquals => Ok(current_value != expected),
            ProfileConditionType::EqualsAny | ProfileConditionType::Unrecognized(_) => {
                Err(unexpected(condition))
            }
        };
    }

    Ok(false)
}

fn is_string_satisfied(
    condition: &ProfileCondition,
    current_value: Option<&str>,
) -> Result<bool, StreamBuilderError> {
    let current_value = current_value.unwrap_or("");
    if current_value.is_empty() {
        return Ok(!condition.is_required);
    }

    let expected = condition.value.as_str();

    match condition.condition {
        ProfileConditionType::EqualsAny => Ok(expected
            .split('|')
            .any(|v| v.eq_ignore_ascii_case(current_value))),
        ProfileConditionType::Equals => Ok(current_value.eq_ignore_ascii_case(expected)),
        ProfileConditionType::NotEquals => Ok(!current_value.eq_ignore_ascii_case(expected)),
        _ => Err(unexpected(condition)),
    }
}

fn is_bool_satisfied(
    condition: &ProfileCondition,
    current_value: Option<bool>,
) -> Result<bool, StreamBuilderError> {
    let Some(current_value) = current_value else {
        return Ok(!condition.is_required);
    };

    if let Ok(expected) = condition.value.parse::<bool>() {
        return match condition.condition {
            ProfileConditionType::Equals => Ok(current_value == expected),
            ProfileConditionType::NotEquals => Ok(current_value != expected),
            _ => Err(unexpected(condition)),
        };
    }

    Ok(false)
}

// Faithful port of C# double comparisons (`double.Equals`), which are exact.
#[allow(clippy::float_cmp)]
fn is_double_satisfied(
    condition: &ProfileCondition,
    current_value: Option<f64>,
) -> Result<bool, StreamBuilderError> {
    let Some(current_value) = current_value else {
        return Ok(!condition.is_required);
    };

    if condition.condition == ProfileConditionType::EqualsAny {
        for single in condition.value.split('|') {
            if single.trim().parse::<f64>() == Ok(current_value) {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    if let Ok(expected) = condition.value.parse::<f64>() {
        return match condition.condition {
            ProfileConditionType::Equals => Ok(current_value == expected),
            ProfileConditionType::GreaterThanEqual => Ok(current_value >= expected),
            ProfileConditionType::LessThanEqual => Ok(current_value <= expected),
            ProfileConditionType::NotEquals => Ok(current_value != expected),
            ProfileConditionType::EqualsAny | ProfileConditionType::Unrecognized(_) => {
                Err(unexpected(condition))
            }
        };
    }

    Ok(false)
}

fn is_timestamp_satisfied(
    condition: &ProfileCondition,
    timestamp: Option<TransportStreamTimestamp>,
) -> Result<bool, StreamBuilderError> {
    let Some(timestamp) = timestamp else {
        return Ok(!condition.is_required);
    };

    let expected = parse_timestamp(&condition.value)?;

    match condition.condition {
        ProfileConditionType::Equals => Ok(timestamp == expected),
        ProfileConditionType::NotEquals => Ok(timestamp != expected),
        _ => Err(unexpected(condition)),
    }
}

/// `Enum.Parse<TransportStreamTimestamp>(value, true)`: a member name in any
/// case or a number, either with surrounding white space. Anything else is
/// the `ArgumentException` it throws (a number beyond `i32` included, where
/// upstream throws an `OverflowException` instead).
fn parse_timestamp(value: &str) -> Result<TransportStreamTimestamp, StreamBuilderError> {
    let trimmed = value.trim();
    for (name, timestamp) in [
        ("None", TransportStreamTimestamp::None),
        ("Zero", TransportStreamTimestamp::Zero),
        ("Valid", TransportStreamTimestamp::Valid),
    ] {
        if trimmed.eq_ignore_ascii_case(name) {
            return Ok(timestamp);
        }
    }
    match trimmed.parse::<i32>() {
        Ok(0) => Ok(TransportStreamTimestamp::None),
        Ok(1) => Ok(TransportStreamTimestamp::Zero),
        Ok(2) => Ok(TransportStreamTimestamp::Valid),
        Ok(other) => Ok(TransportStreamTimestamp::Unrecognized(other)),
        Err(_) => Err(StreamBuilderError::UnknownTimestamp(value.to_owned())),
    }
}

fn is_range_satisfied(
    condition: &ProfileCondition,
    current_value: Option<VideoRangeType>,
) -> Result<bool, StreamBuilderError> {
    let Some(current_value) = current_value.filter(|v| *v != VideoRangeType::Unknown) else {
        return Ok(!condition.is_required);
    };

    // Special case: HDR10 also satisfies if the video is HDR10Plus
    if current_value == VideoRangeType::Hdr10Plus
        && is_range_satisfied_value(condition, VideoRangeType::Hdr10)?
    {
        return Ok(true);
    }

    is_range_satisfied_value(condition, current_value)
}

fn is_range_satisfied_value(
    condition: &ProfileCondition,
    current_value: VideoRangeType,
) -> Result<bool, StreamBuilderError> {
    if condition.condition == ProfileConditionType::EqualsAny {
        for single in condition.value.split('|') {
            if let Some(v) = parse_video_range_type(single)
                && v == current_value
            {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    if let Some(expected) = parse_video_range_type(&condition.value) {
        return match condition.condition {
            ProfileConditionType::Equals => Ok(current_value == expected),
            ProfileConditionType::NotEquals => Ok(current_value != expected),
            _ => Err(unexpected(condition)),
        };
    }

    Ok(false)
}

pub(crate) use video_range_type_name as range_type_name;

#[cfg(test)]
mod tests {
    use super::*;
    use ProfileConditionType::{Equals, LessThanEqual, NotEquals};

    fn condition(
        condition: ProfileConditionType,
        property: ProfileConditionValue,
        value: &str,
    ) -> ProfileCondition {
        ProfileCondition::new(condition, property, value.to_owned())
    }

    /// `ConditionProcessor.IsVideoConditionSatisfied` with only the profile and
    /// timestamp set.
    fn video(
        condition: &ProfileCondition,
        profile: Option<&str>,
        timestamp: Option<TransportStreamTimestamp>,
    ) -> Result<bool, StreamBuilderError> {
        ConditionProcessor::is_video_condition_satisfied(
            condition, None, None, None, None, profile, None, None, None, None, timestamp, None,
            None, None, 1, None, None, None, None, None,
        )
    }

    /// A property an audio file or track has none of is upstream's
    /// `ArgumentException`, a 400.
    #[test]
    fn a_property_the_stream_lacks_is_an_invalid_argument() {
        let width = condition(Equals, ProfileConditionValue::Width, "1");
        for result in [
            ConditionProcessor::is_audio_condition_satisfied(&width, None, None, None, None),
            ConditionProcessor::is_video_audio_condition_satisfied(
                &width, None, None, None, None, None, None,
            ),
        ] {
            let error = result.expect_err("no width on audio");
            assert_eq!(
                error,
                StreamBuilderError::UnexpectedProperty {
                    stream: "audio",
                    property: ProfileConditionValue::Width
                }
            );
            assert!(error.is_invalid_argument());
        }
        let bitrate = condition(Equals, ProfileConditionValue::AudioBitrate, "1");
        assert!(
            ConditionProcessor::is_image_condition_satisfied(&bitrate, None, None)
                .is_err_and(|e| e.is_invalid_argument())
        );
    }

    /// A comparison the value's type has none of is upstream's
    /// `InvalidOperationException`, a 500 — thrown only once a value is there
    /// to compare, as upstream returns `!IsRequired` for an unknown one first.
    #[test]
    fn a_comparison_the_value_lacks_is_an_invalid_operation() {
        let profile = condition(LessThanEqual, ProfileConditionValue::VideoProfile, "high");
        let error = video(&profile, Some("main"), None).expect_err("no order on strings");
        assert_eq!(
            error,
            StreamBuilderError::UnexpectedConditionType(LessThanEqual)
        );
        assert!(!error.is_invalid_argument());
        assert_eq!(video(&profile, None, None), Ok(true));

        let interlaced = condition(LessThanEqual, ProfileConditionValue::IsInterlaced, "true");
        assert!(
            ConditionProcessor::is_video_condition_satisfied(
                &interlaced,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(true),
                None,
                1,
                None,
                None,
                None,
                None,
                None,
            )
            .is_err_and(|e| !e.is_invalid_argument())
        );

        let unnamed = condition(
            ProfileConditionType::Unrecognized(9),
            ProfileConditionValue::NumStreams,
            "1",
        );
        assert_eq!(
            video(&unnamed, None, None),
            Err(StreamBuilderError::UnexpectedConditionType(
                ProfileConditionType::Unrecognized(9)
            ))
        );
    }

    /// The timestamp is read with `Enum.Parse(value, ignoreCase: true)`: any
    /// case or a number parses, anything else is its `ArgumentException`.
    #[test]
    fn a_timestamp_condition_parses_its_value_as_enum_parse_does() {
        let valid = Some(TransportStreamTimestamp::Valid);
        for value in ["Valid", "valid", " VALID ", "2"] {
            let c = condition(Equals, ProfileConditionValue::VideoTimestamp, value);
            assert_eq!(video(&c, None, valid), Ok(true), "{value}");
        }
        let undefined = condition(NotEquals, ProfileConditionValue::VideoTimestamp, "7");
        assert_eq!(video(&undefined, None, valid), Ok(true));

        let unknown = condition(Equals, ProfileConditionValue::VideoTimestamp, "sometimes");
        let error = video(&unknown, None, valid).expect_err("no such timestamp");
        assert_eq!(
            error,
            StreamBuilderError::UnknownTimestamp("sometimes".to_owned())
        );
        assert!(error.is_invalid_argument());
        // An unknown timestamp is never compared, so nothing throws.
        assert_eq!(video(&unknown, None, None), Ok(true));
    }
}
