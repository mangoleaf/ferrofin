//! Bitstream filters: container fixups, and stripping dynamic HDR metadata a
//! client cannot handle.
//!
//! Port of C# `EncodingHelper.ShouldRemoveDynamicHdrMetadata`,
//! `CanEncoderRemoveDynamicHdrMetadata`, `IsDoviRemoved`, `IsHdr10PlusRemoved`
//! and `GetBitStreamArgs` (10.11.z 1403-1540).
//!
//! **Why this exists at all: it is what lets a Dolby Vision file stream-copy to
//! a client that cannot play it.** Without these filters the only options are
//! sending metadata the client will choke on, or a full re-encode of a 4K HDR
//! source. Stripping a few bytes of side data from an otherwise untouched
//! stream is enormously cheaper than either.
//!
//! There are two removals and they are not symmetric:
//!
//! - **Dolby Vision** can be stripped from HEVC or AV1, and there are two ways
//!   to do it — the codec-specific `*_metadata=remove_dovi=1`, or the generic
//!   `dovi_rpu=strip=1` as a fallback for builds without it.
//! - **HDR10+** has only the codec-specific filter, so a build lacking it
//!   cannot strip HDR10+ at all — and the copy is refused rather than sent
//!   wrong.
//!
//! The decision reads the *client's* declared range types, not the server's
//! preferences: a player that never mentions Dolby Vision is deliberately
//! allowed to copy a broken DOVI configuration, because an HDR10 player ignores
//! the metadata it does not understand rather than crashing on it.

use ferrofin_model::data::{VideoRange, VideoRangeType};
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;

use super::hw::capabilities::{BsfOption, FfmpegCapabilities};
use super::transcode_state::EncodingJobInfo;

/// What, if anything, has to be stripped from the bitstream.
/// Port of C# `DynamicHdrMetadataRemovalPlan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrMetadataRemoval {
    /// Copy the stream through untouched.
    None,
    /// Strip the Dolby Vision RPU.
    RemoveDovi,
    /// Strip the HDR10+ dynamic metadata.
    RemoveHdr10Plus,
}

/// Whether `stream` is H.264. Port of `IsH264`.
fn is_h264(stream: Option<&MediaStream>) -> bool {
    codec_contains(stream, &["264", "avc"])
}

/// Whether `stream` is HEVC. Port of `IsH265`.
fn is_h265(stream: Option<&MediaStream>) -> bool {
    codec_contains(stream, &["265", "hevc"])
}

/// Whether `stream` is AV1. Port of `IsAv1`.
fn is_av1(stream: Option<&MediaStream>) -> bool {
    codec_contains(stream, &["av1"])
}

/// Whether `stream` is AAC. Port of `IsAAC`.
fn is_aac(stream: Option<&MediaStream>) -> bool {
    codec_contains(stream, &["aac"])
}

/// Whether the codec name **contains** any of `needles`, case-insensitively.
///
/// A substring test, not equality — upstream matches `"264"` and `"avc"`, so
/// `h264`, `avc1` and `libx264` all count as H.264. Note this is a *different*
/// notion of "contains" from the one the range-type check uses twenty lines
/// away, which is element-wise over a list.
fn codec_contains(stream: Option<&MediaStream>, needles: &[&str]) -> bool {
    let Some(codec) = stream.and_then(|s| s.codec.as_deref()) else {
        return false;
    };
    let lower = codec.to_ascii_lowercase();
    needles.iter().any(|n| lower.contains(n))
}

/// What has to be stripped for this client. Port of
/// `ShouldRemoveDynamicHdrMetadata`.
///
/// Assumes the plain range check has already run — the trivial fallbacks
/// (HDR10+ → HDR10, DOVIWithHDR10 → HDR10) are decided elsewhere, and this only
/// sees what survives them.
#[must_use]
pub fn should_remove_dynamic_hdr_metadata(state: &EncodingJobInfo) -> HdrMetadataRemoval {
    let Some(video) = state.video_stream.as_ref() else {
        return HdrMetadataRemoval::None;
    };
    if video.video_range != Some(VideoRange::Hdr) {
        return HdrMetadataRemoval::None;
    }
    let requested = state.requested_range_types(video.codec.as_deref().unwrap_or_default());
    if requested.is_empty() {
        // The client said nothing about ranges, so there is nothing to adapt
        // to. Copying through is the conservative answer.
        return HdrMetadataRemoval::None;
    }
    let wants = |t: VideoRangeType| {
        let name = range_type_name(t);
        requested.iter().any(|r| r.eq_ignore_ascii_case(name))
    };
    let has_hdr10 = wants(VideoRangeType::Hdr10);
    let has_dovi = wants(VideoRangeType::Dovi);
    let has_dovi_with_el = wants(VideoRangeType::DoviWithEl);
    let has_dovi_with_el_hdr10plus = wants(VideoRangeType::DoviWithElhdr10Plus);
    let range_type = video.video_range_type;

    let mut remove_hdr10plus = false;
    // Case 1: the client plays HDR10 but not an enhancement layer, and this
    // file has one.
    let mut remove_dovi =
        !has_dovi_with_el && has_hdr10 && range_type == Some(VideoRangeType::DoviWithEl);

    // Case 2: a Dolby Vision player refuses a broken DOVI configuration. A
    // client that never claimed DOVI is deliberately left alone — an HDR10
    // player ignores metadata it does not understand rather than crashing, so
    // copying the bad data through is safe and cheaper.
    remove_dovi = remove_dovi || (has_dovi && range_type == Some(VideoRangeType::DoviInvalid));

    // Both an enhancement layer and HDR10+ in one file. A client that handles
    // EL but not the combination loses HDR10+; anything else loses DOVI.
    if range_type == Some(VideoRangeType::DoviWithElhdr10Plus) {
        remove_hdr10plus = has_dovi_with_el && !has_dovi_with_el_hdr10plus;
        remove_dovi = remove_dovi || !remove_hdr10plus;
    }

    if remove_dovi {
        return HdrMetadataRemoval::RemoveDovi;
    }

    // A Dolby Vision player is confused by coexisting HDR10+ metadata.
    remove_hdr10plus =
        remove_hdr10plus || (has_dovi && range_type == Some(VideoRangeType::DoviWithHdr10Plus));
    if remove_hdr10plus {
        HdrMetadataRemoval::RemoveHdr10Plus
    } else {
        HdrMetadataRemoval::None
    }
}

/// The wire name of a range type, as the client spells it in its profile.
fn range_type_name(t: VideoRangeType) -> &'static str {
    match t {
        VideoRangeType::Unknown => "Unknown",
        VideoRangeType::Sdr => "SDR",
        VideoRangeType::Hdr10 => "HDR10",
        VideoRangeType::Hlg => "HLG",
        VideoRangeType::Dovi => "DOVI",
        VideoRangeType::DoviWithHdr10 => "DOVIWithHDR10",
        VideoRangeType::DoviWithHlg => "DOVIWithHLG",
        VideoRangeType::DoviWithSdr => "DOVIWithSDR",
        VideoRangeType::DoviWithEl => "DOVIWithEL",
        VideoRangeType::DoviWithHdr10Plus => "DOVIWithHDR10Plus",
        VideoRangeType::DoviWithElhdr10Plus => "DOVIWithELHDR10Plus",
        VideoRangeType::DoviInvalid => "DOVIInvalid",
        VideoRangeType::Hdr10Plus => "HDR10Plus",
    }
}

/// Whether this ffmpeg can perform `plan` on `video_stream`. Port of
/// `CanEncoderRemoveDynamicHdrMetadata`.
///
/// A `None` plan is trivially possible. The asymmetry between the two removals
/// lives here: Dolby Vision has a generic fallback filter, HDR10+ does not.
#[must_use]
pub fn can_remove_dynamic_hdr_metadata(
    caps: &FfmpegCapabilities,
    plan: HdrMetadataRemoval,
    video_stream: Option<&MediaStream>,
) -> bool {
    match plan {
        HdrMetadataRemoval::None => true,
        HdrMetadataRemoval::RemoveDovi => {
            caps.supports_bsf_with_option(BsfOption::DoviRpuStrip)
                || (is_h265(video_stream)
                    && caps.supports_bsf_with_option(BsfOption::HevcMetadataRemoveDovi))
                || (is_av1(video_stream)
                    && caps.supports_bsf_with_option(BsfOption::Av1MetadataRemoveDovi))
        }
        HdrMetadataRemoval::RemoveHdr10Plus => {
            (is_h265(video_stream)
                && caps.supports_bsf_with_option(BsfOption::HevcMetadataRemoveHdr10Plus))
                || (is_av1(video_stream)
                    && caps.supports_bsf_with_option(BsfOption::Av1MetadataRemoveHdr10Plus))
        }
    }
}

/// Whether Dolby Vision will actually be stripped. Port of `IsDoviRemoved`.
///
/// Both halves matter: the client has to need it removed **and** this ffmpeg
/// has to be able to. A build that cannot is why the copy gets refused instead.
#[must_use]
pub fn is_dovi_removed(caps: &FfmpegCapabilities, state: &EncodingJobInfo) -> bool {
    state.video_stream.is_some()
        && should_remove_dynamic_hdr_metadata(state) == HdrMetadataRemoval::RemoveDovi
        && can_remove_dynamic_hdr_metadata(
            caps,
            HdrMetadataRemoval::RemoveDovi,
            state.video_stream.as_ref(),
        )
}

/// Whether HDR10+ will actually be stripped. Port of `IsHdr10PlusRemoved`.
#[must_use]
pub fn is_hdr10_plus_removed(caps: &FfmpegCapabilities, state: &EncodingJobInfo) -> bool {
    state.video_stream.is_some()
        && should_remove_dynamic_hdr_metadata(state) == HdrMetadataRemoval::RemoveHdr10Plus
        && can_remove_dynamic_hdr_metadata(
            caps,
            HdrMetadataRemoval::RemoveHdr10Plus,
            state.video_stream.as_ref(),
        )
}

/// The `-bsf` argument for a stream, or `None`. Port of `GetBitStreamArgs`.
///
/// Two unrelated jobs share this function upstream, and both only apply to a
/// **stream copy** — a re-encode produces a fresh bitstream that needs neither:
///
/// - **Container fixups.** `h264_mp4toannexb` / `hevc_mp4toannexb` convert
///   length-prefixed MP4 NAL units to the start-code form mpegts wants, and
///   `aac_adtstoasc` converts the other way for audio.
/// - **HDR metadata removal**, appended to the HEVC/AV1 filter chain.
#[must_use]
pub fn bit_stream_args(
    caps: &FfmpegCapabilities,
    state: &EncodingJobInfo,
    stream_type: MediaStreamType,
) -> Option<String> {
    let stream = match stream_type {
        MediaStreamType::Audio => state.audio_stream.as_ref(),
        _ => state.video_stream.as_ref(),
    };

    if is_h264(stream) {
        // Upstream's own note: the mpegts muxer inserts this itself, so it may
        // be redundant. Kept because the argument is what parity is measured on.
        return Some("-bsf:v h264_mp4toannexb".to_owned());
    }
    if is_aac(stream) {
        return Some("-bsf:a aac_adtstoasc".to_owned());
    }

    if is_h265(stream) {
        let mut filter = "-bsf:v hevc_mp4toannexb".to_owned();
        // No capability re-check here, deliberately: the copy would already
        // have been refused if this ffmpeg could not do the removal, and a bsf
        // only appears on a copy at all.
        match should_remove_dynamic_hdr_metadata(state) {
            HdrMetadataRemoval::None => {}
            HdrMetadataRemoval::RemoveDovi => {
                filter.push_str(
                    if caps.supports_bsf_with_option(BsfOption::HevcMetadataRemoveDovi) {
                        ",hevc_metadata=remove_dovi=1"
                    } else {
                        // The generic stripper, for builds without the
                        // codec-specific option.
                        ",dovi_rpu=strip=1"
                    },
                );
            }
            HdrMetadataRemoval::RemoveHdr10Plus => {
                filter.push_str(",hevc_metadata=remove_hdr10plus=1");
            }
        }
        return Some(filter);
    }

    if is_av1(stream) {
        // AV1 needs no container fixup, so unlike HEVC there is nothing to
        // return when no metadata has to go.
        return match should_remove_dynamic_hdr_metadata(state) {
            HdrMetadataRemoval::None => None,
            HdrMetadataRemoval::RemoveDovi => Some(
                if caps.supports_bsf_with_option(BsfOption::Av1MetadataRemoveDovi) {
                    "-bsf:v av1_metadata=remove_dovi=1"
                } else {
                    "-bsf:v dovi_rpu=strip=1"
                }
                .to_owned(),
            ),
            HdrMetadataRemoval::RemoveHdr10Plus => {
                Some("-bsf:v av1_metadata=remove_hdr10plus=1".to_owned())
            }
        };
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding_helper::hw::capabilities::FfmpegCapabilities;
    use ferrofin_model::entities::MediaStreamType;
    use rstest::rstest;

    // The truth table below was derived from the C# (10.11.z 1403-1447) by a
    // transliteration written without reference to this file.

    fn video(codec: &str, range_type: VideoRangeType) -> MediaStream {
        MediaStream {
            codec: Some(codec.to_owned()),
            index: 0,
            stream_type: MediaStreamType::Video,
            video_range: Some(VideoRange::Hdr),
            video_range_type: Some(range_type),
            ..MediaStream::default()
        }
    }

    /// A job carrying `streams`, with nothing else set that matters here.
    fn job(streams: Vec<MediaStream>) -> EncodingJobInfo {
        EncodingJobInfo {
            display: crate::encoding_helper::transcode_state::TranscodeDisplayNames::default(),
            base_request: crate::BaseEncodingJobOptions::default(),
            video_stream: streams
                .iter()
                .find(|s| s.stream_type == MediaStreamType::Video)
                .cloned(),
            audio_stream: streams
                .iter()
                .find(|s| s.stream_type == MediaStreamType::Audio)
                .cloned(),
            subtitle_stream: None,
            media_source: ferrofin_model::dto::MediaSourceInfo {
                media_streams: streams,
                ..ferrofin_model::dto::MediaSourceInfo::default()
            },
            output_video_codec: None,
            output_audio_codec: None,
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: None,
            output_container: None,
            output_video_sync: None,
            output_file_path: "/tmp/out.mp4".to_owned(),
            input_container: None,
            is_input_video: true,
            subtitle_delivery_method: ferrofin_model::dlna::enums::SubtitleDeliveryMethod::Encode,
            run_time_ticks: Some(1),
            transcoding_type: ferrofin_traits::media_encoding::TranscodingJobType::Progressive,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            segment_length_secs: 0,
            wait_for_path: None,
            segment_container: None,
            play_session_id: None,
            device_id: None,
        }
    }

    fn state_with(stream: MediaStream, requested: &[&str]) -> EncodingJobInfo {
        let mut state = job(vec![stream]);
        state.base_request.video_range_type = if requested.is_empty() {
            None
        } else {
            Some(requested.join(","))
        };
        state
    }

    fn caps_with(options: &[BsfOption]) -> FfmpegCapabilities {
        let mut b = FfmpegCapabilities::builder();
        for o in options {
            b = b.bsf_option(*o, true);
        }
        b.build()
    }

    fn all_bsf() -> FfmpegCapabilities {
        caps_with(&[
            BsfOption::DoviRpuStrip,
            BsfOption::HevcMetadataRemoveDovi,
            BsfOption::HevcMetadataRemoveHdr10Plus,
            BsfOption::Av1MetadataRemoveDovi,
            BsfOption::Av1MetadataRemoveHdr10Plus,
        ])
    }

    #[rstest]
    // The only non-`None` cells of the full matrix. Everything else copies
    // through untouched.
    //
    // An enhancement layer the client cannot play, but it can play HDR10.
    #[case(VideoRangeType::DoviWithEl, &["HDR10"], HdrMetadataRemoval::RemoveDovi)]
    #[case(VideoRangeType::DoviWithEl, &["HDR10", "DOVI"], HdrMetadataRemoval::RemoveDovi)]
    // ...but plain DOVI does NOT rescue it: case 1 keys on HDR10 specifically.
    #[case(VideoRangeType::DoviWithEl, &["DOVI"], HdrMetadataRemoval::None)]
    #[case(VideoRangeType::DoviWithEl, &["DOVIWithEL", "HDR10"], HdrMetadataRemoval::None)]
    // A Dolby Vision player refuses a broken configuration...
    #[case(VideoRangeType::DoviInvalid, &["DOVI"], HdrMetadataRemoval::RemoveDovi)]
    // ...but `DOVIWithEL` is a different string, so it does not trigger case 2,
    // and a client that never claimed DOVI copies the bad data through happily.
    #[case(VideoRangeType::DoviInvalid, &["DOVIWithEL"], HdrMetadataRemoval::None)]
    #[case(VideoRangeType::DoviInvalid, &["HDR10"], HdrMetadataRemoval::None)]
    // A DOVI player is confused by coexisting HDR10+.
    #[case(VideoRangeType::DoviWithHdr10Plus, &["DOVI"], HdrMetadataRemoval::RemoveHdr10Plus)]
    #[case(VideoRangeType::DoviWithHdr10Plus, &["HDR10"], HdrMetadataRemoval::None)]
    // The outlier row: once the list is non-empty, the ONLY escape from
    // RemoveDovi is naming DOVIWithEL and NOT DOVIWithELHDR10Plus.
    #[case(VideoRangeType::DoviWithElhdr10Plus, &["DOVIWithEL"], HdrMetadataRemoval::RemoveHdr10Plus)]
    #[case(VideoRangeType::DoviWithElhdr10Plus, &["DOVIWithEL", "HDR10"], HdrMetadataRemoval::RemoveHdr10Plus)]
    #[case(VideoRangeType::DoviWithElhdr10Plus, &["HDR10"], HdrMetadataRemoval::RemoveDovi)]
    #[case(VideoRangeType::DoviWithElhdr10Plus, &["DOVI"], HdrMetadataRemoval::RemoveDovi)]
    #[case(VideoRangeType::DoviWithElhdr10Plus, &["SDR"], HdrMetadataRemoval::RemoveDovi)]
    // Declaring the EXACT format still strips it — naming both spellings is
    // what a client has to do, which reads backwards but is upstream's rule.
    #[case(VideoRangeType::DoviWithElhdr10Plus, &["DOVIWithELHDR10Plus"], HdrMetadataRemoval::RemoveDovi)]
    #[case(VideoRangeType::DoviWithElhdr10Plus, &["DOVIWithEL", "DOVIWithELHDR10Plus"], HdrMetadataRemoval::RemoveDovi)]
    // Ordinary HDR needs nothing removed whatever the client says.
    #[case(VideoRangeType::Hdr10, &["DOVI"], HdrMetadataRemoval::None)]
    #[case(VideoRangeType::Hdr10Plus, &["DOVI"], HdrMetadataRemoval::None)]
    #[case(VideoRangeType::Dovi, &["HDR10"], HdrMetadataRemoval::None)]
    #[case(VideoRangeType::DoviWithHdr10, &["HDR10"], HdrMetadataRemoval::None)]
    fn the_removal_plan_follows_what_the_client_declared(
        #[case] range_type: VideoRangeType,
        #[case] requested: &[&str],
        #[case] expected: HdrMetadataRemoval,
    ) {
        let state = state_with(video("hevc", range_type), requested);
        assert_eq!(
            should_remove_dynamic_hdr_metadata(&state),
            expected,
            "{range_type:?} + {requested:?}"
        );
    }

    #[test]
    fn a_client_that_declares_no_ranges_copies_anything_through() {
        // Including a broken DOVI configuration. Deliberate: an HDR10 player
        // ignores metadata it does not understand rather than crashing, so
        // there is nothing to protect it from.
        for rt in [
            VideoRangeType::DoviInvalid,
            VideoRangeType::DoviWithElhdr10Plus,
            VideoRangeType::DoviWithEl,
        ] {
            let state = state_with(video("hevc", rt), &[]);
            assert_eq!(
                should_remove_dynamic_hdr_metadata(&state),
                HdrMetadataRemoval::None,
                "{rt:?}"
            );
        }
    }

    #[test]
    fn an_sdr_stream_never_reaches_the_decision_at_all() {
        // The range check comes first, before the client's list is even read.
        let mut stream = video("hevc", VideoRangeType::DoviWithElhdr10Plus);
        stream.video_range = Some(VideoRange::Sdr);
        let state = state_with(stream, &["HDR10"]);
        assert_eq!(
            should_remove_dynamic_hdr_metadata(&state),
            HdrMetadataRemoval::None
        );
    }

    #[test]
    fn the_range_type_match_is_a_whole_element_not_a_substring() {
        // `DOVIWithELHDR10Plus` must NOT satisfy a search for `DOVIWithEL`.
        // A substring implementation flips this cell and nothing else, which is
        // why it would survive until a real client hit it.
        let state = state_with(
            video("hevc", VideoRangeType::DoviWithElhdr10Plus),
            &["DOVIWithELHDR10Plus"],
        );
        assert_eq!(
            should_remove_dynamic_hdr_metadata(&state),
            HdrMetadataRemoval::RemoveDovi
        );
    }

    #[test]
    fn the_codec_match_is_a_substring_unlike_the_range_type_one() {
        // Two different notions of "contains" twenty lines apart upstream.
        for codec in ["h264", "avc1", "libx264", "H264"] {
            let state = state_with(video(codec, VideoRangeType::Hdr10), &[]);
            assert_eq!(
                bit_stream_args(&all_bsf(), &state, MediaStreamType::Video).as_deref(),
                Some("-bsf:v h264_mp4toannexb"),
                "{codec}"
            );
        }
    }

    // ----- capability gating -------------------------------------------------

    #[test]
    fn hdr10_plus_has_no_generic_fallback_the_way_dolby_vision_does() {
        // `dovi_rpu=strip=1` strips the Dolby Vision RPU, which is not where
        // HDR10+ lives — so a build with only that filter can strip DOVI from
        // anything but cannot strip HDR10+ at all, and the copy is refused
        // instead of being sent wrong.
        let only_rpu = caps_with(&[BsfOption::DoviRpuStrip]);
        let hevc = video("hevc", VideoRangeType::DoviWithHdr10Plus);
        assert!(can_remove_dynamic_hdr_metadata(
            &only_rpu,
            HdrMetadataRemoval::RemoveDovi,
            Some(&hevc)
        ));
        assert!(!can_remove_dynamic_hdr_metadata(
            &only_rpu,
            HdrMetadataRemoval::RemoveHdr10Plus,
            Some(&hevc)
        ));
    }

    #[test]
    fn the_generic_stripper_is_accepted_for_any_codec() {
        // No codec guard on the `dovi_rpu` arm, so it answers `true` even for a
        // stream whose bitstream args will turn out to be nothing at all.
        let only_rpu = caps_with(&[BsfOption::DoviRpuStrip]);
        let vp9 = video("vp9", VideoRangeType::DoviInvalid);
        assert!(can_remove_dynamic_hdr_metadata(
            &only_rpu,
            HdrMetadataRemoval::RemoveDovi,
            Some(&vp9)
        ));
        // ...and indeed nothing is emitted for it.
        let state = state_with(vp9, &["DOVI"]);
        assert_eq!(
            bit_stream_args(&only_rpu, &state, MediaStreamType::Video),
            None
        );
    }

    #[test]
    fn removal_is_reported_only_when_it_will_actually_happen() {
        // Both halves: the client has to need it AND ffmpeg has to be able.
        let state = state_with(video("hevc", VideoRangeType::DoviInvalid), &["DOVI"]);
        assert!(is_dovi_removed(&all_bsf(), &state));
        assert!(!is_dovi_removed(&caps_with(&[]), &state));

        let plus = state_with(video("hevc", VideoRangeType::DoviWithHdr10Plus), &["DOVI"]);
        assert!(is_hdr10_plus_removed(&all_bsf(), &plus));
        assert!(!is_hdr10_plus_removed(
            &caps_with(&[BsfOption::DoviRpuStrip]),
            &plus
        ));
    }

    // ----- the emitted arguments ---------------------------------------------

    #[rstest]
    #[case("aac", MediaStreamType::Audio, Some("-bsf:a aac_adtstoasc"))]
    #[case("mp3", MediaStreamType::Audio, None)]
    fn audio_gets_its_container_fixup(
        #[case] codec: &str,
        #[case] kind: MediaStreamType,
        #[case] expected: Option<&str>,
    ) {
        let state = job(vec![MediaStream {
            codec: Some(codec.to_owned()),
            index: 1,
            stream_type: MediaStreamType::Audio,
            ..MediaStream::default()
        }]);
        assert_eq!(
            bit_stream_args(&all_bsf(), &state, kind).as_deref(),
            expected
        );
    }

    #[test]
    fn hevc_always_gets_a_filter_and_av1_only_when_it_needs_one() {
        // Asymmetric by design: AV1 has no `mp4toannexb` equivalent, so with
        // nothing to strip it gets no `-bsf` at all.
        let caps = all_bsf();
        let hevc = state_with(video("hevc", VideoRangeType::Hdr10), &["HDR10"]);
        assert_eq!(
            bit_stream_args(&caps, &hevc, MediaStreamType::Video).as_deref(),
            Some("-bsf:v hevc_mp4toannexb")
        );
        let av1 = state_with(video("av1", VideoRangeType::Hdr10), &["HDR10"]);
        assert_eq!(bit_stream_args(&caps, &av1, MediaStreamType::Video), None);
    }

    #[rstest]
    // The removal is comma-appended to the same `-bsf:v`, making one filter
    // chain rather than a second flag.
    #[case("hevc", VideoRangeType::DoviInvalid, &["DOVI"], true,
        "-bsf:v hevc_mp4toannexb,hevc_metadata=remove_dovi=1")]
    #[case("hevc", VideoRangeType::DoviInvalid, &["DOVI"], false,
        "-bsf:v hevc_mp4toannexb,dovi_rpu=strip=1")]
    #[case("hevc", VideoRangeType::DoviWithHdr10Plus, &["DOVI"], true,
        "-bsf:v hevc_mp4toannexb,hevc_metadata=remove_hdr10plus=1")]
    #[case("av1", VideoRangeType::DoviInvalid, &["DOVI"], true,
        "-bsf:v av1_metadata=remove_dovi=1")]
    #[case("av1", VideoRangeType::DoviInvalid, &["DOVI"], false,
        "-bsf:v dovi_rpu=strip=1")]
    #[case("av1", VideoRangeType::DoviWithHdr10Plus, &["DOVI"], true,
        "-bsf:v av1_metadata=remove_hdr10plus=1")]
    fn the_removal_is_appended_to_the_container_fixup(
        #[case] codec: &str,
        #[case] range_type: VideoRangeType,
        #[case] requested: &[&str],
        #[case] codec_specific: bool,
        #[case] expected: &str,
    ) {
        // Without the codec-specific option the generic stripper stands in —
        // for Dolby Vision only.
        let caps = if codec_specific {
            all_bsf()
        } else {
            caps_with(&[BsfOption::DoviRpuStrip])
        };
        let state = state_with(video(codec, range_type), requested);
        assert_eq!(
            bit_stream_args(&caps, &state, MediaStreamType::Video).as_deref(),
            Some(expected),
            "{codec}/{range_type:?}"
        );
    }

    #[test]
    fn a_missing_stream_yields_nothing_rather_than_panicking() {
        // Upstream throws a NullReferenceException here: it null-checks the
        // state but not the stream, and a video-only source going to an mp4
        // segment container reaches it with a null audio stream. Returning
        // `None` is the same decision expressed without the crash.
        let state = job(Vec::new());
        assert_eq!(
            bit_stream_args(&all_bsf(), &state, MediaStreamType::Audio),
            None
        );
        assert_eq!(
            bit_stream_args(&all_bsf(), &state, MediaStreamType::Video),
            None
        );
    }
}
