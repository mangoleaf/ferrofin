//! Bitstream filters: container fixups, stripping dynamic HDR metadata a
//! client cannot handle, the HLS copied-audio trim, and the trim of an
//! embedded subtitle sidecar in a seeked stream.
//!
//! Port of C# `EncodingHelper.ShouldRemoveDynamicHdrMetadata`,
//! `CanEncoderRemoveDynamicHdrMetadata`, `IsDoviRemoved`, `IsHdr10PlusRemoved`,
//! `GetBitStreamArgs` (10.11.z 1403-1540), `GetCopiedAudioTrimBsf` and
//! `GetAudioBitStreamArguments`. The sidecar trim has no upstream counterpart
//! ([`embedded_sidecar_trim_arguments`]).
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
//! preferences. Upstream lets a player that never mentions Dolby Vision copy
//! the RPU through, a broken DOVI configuration included, because an HDR10
//! player ignores metadata it does not understand rather than crashing on it.
//!
//! **Divergence (owner decision 2026-10-31, gl-737).** A Dolby Vision profile
//! 8.1 AV1 title copied to jellyfin-web with its RPU did not render, although
//! the browser plays the HDR10 base layer under it. So once a client has
//! declared its range types, Ferrofin strips the RPU from every Dolby Vision
//! stream it copies to a client that listed neither `DOVI` nor the stream's
//! own range type ([`strips_dovi_for_a_client_without_dovi`]), where upstream
//! plans no removal. The copy decision transcodes such a stream when this
//! ffmpeg cannot strip it, or when the job's output takes no bitstream filter
//! ([`video_bit_stream_filters_apply`]).

use ferrofin_model::data::{VideoRange, VideoRangeType};
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::media_encoding::TranscodingJobType;

use super::hw::capabilities::{BsfOption, FfmpegCapabilities};
use super::hw::tonemap::is_dovi;
use super::transcode_state::{EncoderCapabilities, EncodingJobInfo};
use crate::encoder::FfmpegVersion;

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
    let range_type = video.video_range_type();
    // An out-of-spec Dolby Vision stream on an SDR base layer still needs its
    // plan: a Dolby Vision client may not play it (jellyfin#17806).
    if video.video_range() != VideoRange::Hdr && range_type != VideoRangeType::DoviInvalid {
        return HdrMetadataRemoval::None;
    }
    let requested = state.requested_range_types(video.codec.as_deref().unwrap_or_default());
    if requested.is_empty() {
        // The client said nothing about ranges, so there is nothing to adapt
        // to. Copying through is the conservative answer.
        return HdrMetadataRemoval::None;
    }
    let wants = |t: VideoRangeType| {
        requested
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&t.json_name()))
    };
    let has_hdr10 = wants(VideoRangeType::Hdr10);
    let has_dovi = wants(VideoRangeType::Dovi);
    let has_dovi_with_el = wants(VideoRangeType::DoviWithEl);
    let has_dovi_with_el_hdr10plus = wants(VideoRangeType::DoviWithElhdr10Plus);

    let mut remove_hdr10plus = false;
    // Case 1: the client plays HDR10 but not an enhancement layer, and this
    // file has one.
    let mut remove_dovi =
        !has_dovi_with_el && has_hdr10 && range_type == VideoRangeType::DoviWithEl;

    // Case 2: a Dolby Vision player refuses a broken DOVI configuration. A
    // client that never claimed DOVI is deliberately left alone — an HDR10
    // player ignores metadata it does not understand rather than crashing, so
    // copying the bad data through is safe and cheaper.
    remove_dovi = remove_dovi || (has_dovi && range_type == VideoRangeType::DoviInvalid);

    // Both an enhancement layer and HDR10+ in one file. A client that handles
    // EL but not the combination loses HDR10+; anything else loses DOVI.
    if range_type == VideoRangeType::DoviWithElhdr10Plus {
        remove_hdr10plus = has_dovi_with_el && !has_dovi_with_el_hdr10plus;
        remove_dovi = remove_dovi || !remove_hdr10plus;
    }

    if remove_dovi {
        return HdrMetadataRemoval::RemoveDovi;
    }

    // A Dolby Vision player is confused by coexisting HDR10+ metadata.
    remove_hdr10plus =
        remove_hdr10plus || (has_dovi && range_type == VideoRangeType::DoviWithHdr10Plus);
    if remove_hdr10plus {
        HdrMetadataRemoval::RemoveHdr10Plus
    } else {
        HdrMetadataRemoval::None
    }
}

/// Whether Ferrofin strips the Dolby Vision RPU from this copy where upstream
/// copies it through.
///
/// Divergence (owner decision 2026-10-31, gl-737). True for a Dolby Vision
/// stream with a base layer (every Dolby Vision range type but profile 5's
/// `DOVI`, which has none to fall back to) when the client declared range types
/// and listed neither `DOVI` nor the stream's own range type: it plays the base
/// layer, the HDR10, HLG or SDR fallback of profile 8 among them. Upstream
/// plans no removal for most of these and ships the RPU, as it did the
/// Dolby Vision profile 8.1 AV1 title that then did not render in
/// jellyfin-web. False where upstream's
/// [`should_remove_dynamic_hdr_metadata`] strips HDR10+ instead, for an
/// enhancement-layer player, which then gets the range type it listed.
#[must_use]
pub fn strips_dovi_for_a_client_without_dovi(state: &EncodingJobInfo) -> bool {
    let Some(video) = state.video_stream.as_ref() else {
        return false;
    };
    let range_type = video.video_range_type();
    if range_type == VideoRangeType::Dovi || !is_dovi(Some(video)) {
        return false;
    }
    let requested = state.requested_range_types(video.codec.as_deref().unwrap_or_default());
    let wants = |t: VideoRangeType| {
        requested
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&t.json_name()))
    };
    !requested.is_empty()
        && !wants(VideoRangeType::Dovi)
        && !wants(range_type)
        && should_remove_dynamic_hdr_metadata(state) != HdrMetadataRemoval::RemoveHdr10Plus
}

/// What this job strips from a copied video: upstream's
/// [`should_remove_dynamic_hdr_metadata`], plus the RPU of
/// [`strips_dovi_for_a_client_without_dovi`] when this ffmpeg can strip it.
///
/// Without the filters it is upstream's plan alone. The copy decision
/// transcodes such a stream, so only a copy it cannot refuse (the client asked
/// for `copy`, or may not transcode) gets here, and it gets upstream's
/// arguments rather than a filter this ffmpeg does not have.
#[must_use]
pub fn dynamic_hdr_metadata_removal<C: EncoderCapabilities + ?Sized>(
    caps: &C,
    state: &EncodingJobInfo,
) -> HdrMetadataRemoval {
    if strips_dovi_for_a_client_without_dovi(state)
        && can_remove_dynamic_hdr_metadata(
            caps,
            HdrMetadataRemoval::RemoveDovi,
            state.video_stream.as_ref(),
        )
    {
        return HdrMetadataRemoval::RemoveDovi;
    }
    should_remove_dynamic_hdr_metadata(state)
}

/// Whether a copied video's [`bit_stream_args`] reach the ffmpeg command line,
/// and so whether a planned removal happens at all.
///
/// Upstream's gates on the copy's `-bsf:v`, in `DynamicHlsController`'s
/// `GetVideoArguments` and `EncodingHelper.GetProgressiveVideoArguments`: a
/// progressive job takes it only into MPEG-TS, and no job takes it for a source
/// already in Annex B form (`NalLengthSize == "0"`).
#[must_use]
pub fn video_bit_stream_filters_apply(state: &EncodingJobInfo) -> bool {
    (state.transcoding_type != TranscodingJobType::Progressive
        || state
            .output_container
            .as_deref()
            .is_some_and(|container| container.eq_ignore_ascii_case("ts")))
        && state
            .video_stream
            .as_ref()
            .is_some_and(|video| video.nal_length_size.as_deref() != Some("0"))
}

/// Whether this ffmpeg can perform `plan` on `video_stream`. Port of
/// `CanEncoderRemoveDynamicHdrMetadata`.
///
/// A `None` plan is trivially possible. The asymmetry between the two removals
/// lives here: Dolby Vision has a generic fallback filter, HDR10+ does not.
#[must_use]
pub fn can_remove_dynamic_hdr_metadata<C: EncoderCapabilities + ?Sized>(
    caps: &C,
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

/// Whether Dolby Vision will actually be stripped. Port of `IsDoviRemoved`,
/// over the job's plan ([`dynamic_hdr_metadata_removal`]).
///
/// Both halves matter: the client has to need it removed **and** this ffmpeg
/// has to be able to. A build that cannot is why the copy gets refused instead.
#[must_use]
pub fn is_dovi_removed(caps: &FfmpegCapabilities, state: &EncodingJobInfo) -> bool {
    state.video_stream.is_some()
        && dynamic_hdr_metadata_removal(caps, state) == HdrMetadataRemoval::RemoveDovi
        && can_remove_dynamic_hdr_metadata(
            caps,
            HdrMetadataRemoval::RemoveDovi,
            state.video_stream.as_ref(),
        )
}

/// Whether HDR10+ will actually be stripped. Port of `IsHdr10PlusRemoved`,
/// over the job's plan ([`dynamic_hdr_metadata_removal`]).
#[must_use]
pub fn is_hdr10_plus_removed(caps: &FfmpegCapabilities, state: &EncodingJobInfo) -> bool {
    state.video_stream.is_some()
        && dynamic_hdr_metadata_removal(caps, state) == HdrMetadataRemoval::RemoveHdr10Plus
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
/// - **HDR metadata removal**, appended to the HEVC/AV1 filter chain. It
///   follows the job's plan ([`dynamic_hdr_metadata_removal`]), so it also
///   strips the RPU Ferrofin removes where upstream does not.
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
        // No capability re-check here, deliberately: a bsf only appears on a
        // copy, a copy is refused when this ffmpeg cannot do upstream's
        // removal, and a copy that cannot be refused (the client asked for
        // `copy`, or may not transcode) gets Ferrofin's own strip only when
        // this ffmpeg has the filters (`dynamic_hdr_metadata_removal`).
        match dynamic_hdr_metadata_removal(caps, state) {
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
        return match dynamic_hdr_metadata_removal(caps, state) {
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

/// The minimum ffmpeg version for the `noise` bitstream filter's `drop`
/// parameter (ffmpeg 5.0).
const MIN_FFMPEG_NOISE_BSF_DROP: FfmpegVersion = FfmpegVersion::new(5, 0);

/// The `noise` bsf filter that trims copied audio packets before the seek
/// target, achieving the same trim precision as the video decoder's
/// `accurate_seek` without re-encoding audio.
///
/// Port of `GetCopiedAudioTrimBsf`. Returns the bare filter expression
/// (without `-bsf:a`); `None` when the job does not qualify.
///
/// Upstream cuts at the seek on the source's timeline, which its `-copyts`
/// keeps. `timeline_origin_ticks` is the source time the filtered packets
/// count from instead: 0 under `-copyts`, the input seek (`-ss`) without it,
/// so the cutoff is the seek less that origin.
fn copied_audio_trim_bsf(
    state: &EncodingJobInfo,
    timeline_origin_ticks: i64,
    ffmpeg_version: Option<FfmpegVersion>,
) -> Option<String> {
    if state.transcoding_type != TranscodingJobType::Hls
        || !state.is_input_video
        || EncodingJobInfo::is_copy_codec(state.output_video_codec.as_deref())
        || !EncodingJobInfo::is_copy_codec(state.output_audio_codec.as_deref())
        || state
            .input_container
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("wtv"))
        || ffmpeg_version.is_none_or(|v| v < MIN_FFMPEG_NOISE_BSF_DROP)
    {
        return None;
    }
    let ticks = state.base_request.start_time_ticks.filter(|&t| t > 0)?;
    #[allow(clippy::cast_precision_loss, reason = "tick → seconds, f64 is fine")]
    let seek_seconds = ticks.saturating_sub(timeline_origin_ticks) as f64 / 10_000_000.0;
    Some(format!("noise=drop='lt(pts*tb\\,{seek_seconds:.3})'"))
}

/// The `-bsf:a` argument for audio in an HLS or progressive job, combining
/// the copied-audio trim and the `aac_adtstoasc` container fixup.
///
/// Port of `GetAudioBitStreamArguments`. `media_source_container` is the
/// source's container (upstream's `MediaSource.Container`, which is the same
/// as `state.InputContainer` in production but passed separately so the
/// container-fixup check reads it independently of the noise-bsf's wtv
/// gate). `timeline_origin_ticks` is the source time the job's packet
/// timestamps start from (see `copied_audio_trim_bsf`). Returns the full
/// argument (e.g. `"-bsf:a noise=drop='…',aac_adtstoasc"`), ready for
/// `push_split`.
#[must_use]
pub fn audio_bit_stream_arguments(
    state: &EncodingJobInfo,
    segment_container: &str,
    media_source_container: &str,
    timeline_origin_ticks: i64,
    ffmpeg_version: Option<FfmpegVersion>,
) -> Option<String> {
    let mut filters: Vec<&str> = Vec::new();
    let noise = copied_audio_trim_bsf(state, timeline_origin_ticks, ffmpeg_version);
    if let Some(ref f) = noise {
        filters.push(f);
    }
    let segment_is_mp4 = segment_container.eq_ignore_ascii_case("mp4");
    if segment_is_mp4
        && (media_source_container.eq_ignore_ascii_case("ts")
            || media_source_container.eq_ignore_ascii_case("aac")
            || media_source_container.eq_ignore_ascii_case("hls"))
        && is_aac(state.audio_stream.as_ref())
    {
        filters.push("aac_adtstoasc");
    }
    if filters.is_empty() {
        return None;
    }
    Some(format!("-bsf:a {}", filters.join(",")))
}

/// The first ffmpeg whose `setts` filter can rewrite a packet's duration
/// (its `duration` option and `DURATION` constant arrived after 5.0).
const MIN_FFMPEG_SETTS_DURATION: FfmpegVersion = FfmpegVersion::new(5, 1);

/// The external subtitle track `state` embeds into its output, if any.
fn embedded_sidecar(state: &EncodingJobInfo) -> Option<&MediaStream> {
    state.subtitle_stream.as_ref().filter(|sub| {
        state.subtitle_delivery_method == SubtitleDeliveryMethod::Embed && sub.is_external
    })
}

/// The output options that trim an embedded external subtitle to the start
/// of a seeked stream, for the embedded track (`s:0`). `None` when the media
/// file is not seeked (`seeked`: its `-ss` is past 0), the subtitle is not an
/// external track embedded into the output, or the job copies timestamps.
///
/// Divergence, a Jellyfin bug not ported (gl-930): `GetInputArgument` seeks
/// the sidecar with the media file's `-ss`, and a subtitle file's seek lands
/// on the last cue that starts at or before that time, even one already over.
/// Its timestamp is then negative on the output clock, and Matroska, which
/// writes no negative timestamps, moves every stream later by it: a seek to
/// 9 s past a cue at 3.5-8 s started the video 5.5 s into the file (measured
/// on ffmpeg 4.4.2, 8.1.3-Jellyfin and 9.0.2), and the gap grows with the time
/// since the last cue. An MP4 keeps the stale cue at the start instead.
///
/// From ffmpeg 5.1 the cues are trimmed as upstream's
/// `SubtitleEncoder.FilterEvents` trims a sidecar it serves from a start
/// position: the `noise` filter drops a cue that has ended by the start, and
/// `setts` starts one still on screen at the start, its duration cut to the
/// time it has left (measured on 8.1.3-Jellyfin and 9.0.2, a copied track and
/// one converted to ASS or `mov_text` alike). A cue that ends exactly at the
/// start goes too: upstream keeps it with no length, which shows nothing, but
/// MP4's `mov_text` would stretch it to the next cue. ffmpeg 5.0's `setts`
/// cannot change a duration, so there the `noise` filter, upstream's
/// copied-audio trim tool, drops every cue that starts before the start, the
/// one on screen included. An older ffmpeg has no `drop` option, and a
/// version that cannot be read counts as the oldest supported: `-copypriorss`
/// then drops those cues of a copied track.
///
/// Before 5.1 a text sidecar does not reach these options: the server cuts it
/// to the start itself, as `FilterEvents` does, and the job reads that copy
/// unseeked ([`embedded_sidecar_needs_trimmed_copy`], gl-1199). They still
/// serve a text sidecar whose copy could not be made (the planner logs why) and
/// a bitmap one. TODO(gl-1221): before 5.0 a bitmap sidecar converted to
/// another codec (`subtitleCodec`) still moves every stream later, by the start
/// position less its first display set's time (it is read from its start),
/// since `-copypriorss` only reaches stream copy (ffmpeg 4.4.2: from 9 s past a
/// box at 3.5-8 s, PGS converted to `dvdsub` started the video 5.5 s late). Fix
/// path: drop the display sets before the start before ffmpeg converts them,
/// from the sidecar's own timing (the `.sup` segment headers, the `.idx`
/// timestamps, which gl-1215 reads too), on the clock gl-1214 decides.
///
/// A bitmap sidecar is read from its start on every ffmpeg (gl-1200), so every
/// display set before the start reaches these options. TODO(gl-1215): its
/// packets carry no duration, so every one before the start is dropped and a
/// bitmap on screen at the start shows only from the next display set, often
/// its own clear. Fix path: find the last display set at or before the start
/// in the sidecar's own timing (the `.sup` segment headers, the `.idx`
/// timestamps) and start it at 0 instead of dropping it; its time on the
/// output clock follows the clock gl-1214 decides.
///
/// A job that copies timestamps (`-copyts -avoid_negative_ts disabled`) keeps
/// the source's clock, on which nothing before the seek is negative and no
/// stream moves (measured: the drop changes nothing there). The stream routes
/// do not bind `copyTimestamps` yet (gl-1040), so no job does today.
#[must_use]
pub fn embedded_sidecar_trim_arguments(
    state: &EncodingJobInfo,
    seeked: bool,
    ffmpeg_version: Option<FfmpegVersion>,
) -> Option<&'static str> {
    if !seeked
        || embedded_sidecar(state).is_none()
        || state.copies_timestamps()
        || state.trimmed_subtitle_path.is_some()
    {
        return None;
    }
    let at_least = |minimum| ffmpeg_version.is_some_and(|version| version >= minimum);
    Some(if at_least(MIN_FFMPEG_SETTS_DURATION) {
        "-bsf:s:0 noise=drop='lt(pts\\,0)*lte(pts+duration\\,0)',setts=pts='max(PTS\\,0)':dts='max(DTS\\,0)':duration='DURATION+min(PTS\\,0)'"
    } else if at_least(MIN_FFMPEG_NOISE_BSF_DROP) {
        "-bsf:s:0 noise=drop='lt(pts\\,0)'"
    } else {
        "-copypriorss:s:0 0"
    })
}

/// Whether a seeked stream embeds its external text sidecar from a copy the
/// server cuts to the start, rather than from the sidecar seeked like the
/// media file and trimmed on the output side
/// ([`embedded_sidecar_trim_arguments`]). `seeked` as there.
///
/// Before ffmpeg 5.1, or with a version that cannot be read, the output trim
/// cannot cut the sidecar as upstream's `SubtitleEncoder.FilterEvents` cuts
/// one it serves from a start position: it drops the cue on screen at the
/// start instead of starting it at 0, and before 5.0, for a track converted
/// to another codec (`subtitleCodec`), it drops nothing, since `-copypriorss`
/// only reaches stream copy, and every stream starts late by the time since
/// the last cue before the start (ffmpeg 4.4.2: from 8.2 s past a cue at
/// 3.5-8 s, converted to ASS, the video started 4.7 s late; gl-1199). There
/// the server's own subtitle encoder cuts the sidecar with `FilterEvents`
/// instead, and the job reads that copy, not seeked
/// ([`EncodingJobInfo::trimmed_subtitle_path`]). Measured on 4.4.2 from 8.2
/// and 9 s, copied, converted to ASS and to `mov_text`: every stream started
/// on time, and the cue on screen at the start showed from 0. From 5.1 the
/// output trim already cuts as `FilterEvents` does and the sidecar is read as
/// it is. A bitmap sidecar has no text for the encoder to cut.
#[must_use]
pub fn embedded_sidecar_needs_trimmed_copy(
    state: &EncodingJobInfo,
    seeked: bool,
    ffmpeg_version: Option<FfmpegVersion>,
) -> bool {
    seeked
        && embedded_sidecar(state).is_some_and(MediaStream::is_text_subtitle_stream)
        && !state.copies_timestamps()
        && ffmpeg_version.is_none_or(|version| version < MIN_FFMPEG_SETTS_DURATION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding_helper::hw::capabilities::FfmpegCapabilities;
    use crate::encoding_helper::test_streams::{invalid_dovi, with_range};
    use ferrofin_model::entities::MediaStreamType;
    use rstest::rstest;

    // The truth table below was derived from the C# (10.11.z 1403-1447) by a
    // transliteration written without reference to this file.

    fn video(codec: &str, range_type: VideoRangeType) -> MediaStream {
        with_range(
            MediaStream {
                codec: Some(codec.to_owned()),
                index: 0,
                ..MediaStream::default()
            },
            range_type,
        )
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
            trimmed_subtitle_path: None,
            transcode_reasons: ferrofin_model::session::TranscodeReasons::empty(),
            run_time_ticks: Some(1),
            transcoding_type: ferrofin_traits::media_encoding::TranscodingJobType::Progressive,
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
    fn an_sdr_dolby_vision_stream_is_left_alone() {
        // Dolby Vision on an SDR base layer needs nothing removed. The early
        // range check only short-cuts it: no SDR range type meets a removal
        // rule here.
        let state = state_with(video("hevc", VideoRangeType::DoviWithSdr), &["HDR10"]);
        assert_eq!(
            should_remove_dynamic_hdr_metadata(&state),
            HdrMetadataRemoval::None
        );
    }

    // Transliterated from upstream's `EncodingHelperDoviTests` (v12.4,
    // `tests/Jellyfin.Controller.Tests/MediaEncoding/EncodingHelperDoviTests.cs`).
    // Its `CanStreamCopyVideo_InvalidDovi_RequiresRemovalSupportOnlyForDoviClients`
    // is with the copy decision, in `helper.rs`.

    /// Upstream's `CreateState`: a copy of [`invalid_dovi`].
    fn invalid_dovi_state(codec: &str, transfer: Option<&str>) -> EncodingJobInfo {
        let mut state = job(vec![invalid_dovi(codec, transfer)]);
        state.output_video_codec = Some("copy".to_owned());
        state
    }

    /// Port of `GetBitStreamArgs_InvalidDovi_PreservesClientDependentRemoval`.
    /// Upstream strips an invalid stream's Dolby Vision only for a client that
    /// asked for DOVI, whatever its base layer: an SDR base no longer skips the
    /// decision. Three rows differ by a divergence (owner decision 2026-10-31,
    /// gl-737): a client that declared `SDR`, `HDR10` or `DOVIWithEL` without
    /// `DOVI` gets the RPU stripped too, where upstream copies it through. A
    /// client that declared no ranges still gets it untouched.
    #[rstest]
    #[case("hevc", None, "hevc_metadata=remove_dovi=1")]
    #[case("hevc", Some("bt709"), "hevc_metadata=remove_dovi=1")]
    #[case("hevc", Some("smpte2084"), "hevc_metadata=remove_dovi=1")]
    #[case("hevc", Some("arib-std-b67"), "hevc_metadata=remove_dovi=1")]
    #[case("av1", None, "av1_metadata=remove_dovi=1")]
    #[case("av1", Some("bt709"), "av1_metadata=remove_dovi=1")]
    #[case("av1", Some("smpte2084"), "av1_metadata=remove_dovi=1")]
    #[case("av1", Some("arib-std-b67"), "av1_metadata=remove_dovi=1")]
    fn get_bit_stream_args_invalid_dovi_preserves_client_dependent_removal(
        #[case] codec: &str,
        #[case] transfer: Option<&str>,
        #[case] expected: &str,
    ) {
        let mut state = invalid_dovi_state(codec, transfer);
        let removal = all_bsf();
        let no_removal = caps_with(&[]);
        for (requested, remove_dovi) in [
            (None, false),
            // Upstream: false for these three.
            (Some("SDR"), true),
            (Some("HDR10"), true),
            (Some("DOVIWithEL"), true),
            (Some("DOVI"), true),
            (Some("SDR,DOVI"), true),
        ] {
            state.base_request.video_range_type = requested.map(str::to_owned);
            assert_eq!(
                is_dovi_removed(&removal, &state),
                remove_dovi,
                "{requested:?}"
            );
            let args = bit_stream_args(&removal, &state, MediaStreamType::Video);
            if remove_dovi {
                assert!(
                    args.as_deref().is_some_and(|a| a.contains(expected)),
                    "{requested:?}: {args:?}"
                );
            } else {
                assert_eq!(
                    args.as_deref(),
                    (codec == "hevc").then_some("-bsf:v hevc_mp4toannexb"),
                    "{requested:?}"
                );
            }
            assert!(!is_dovi_removed(&no_removal, &state), "{requested:?}");
        }
    }

    #[test]
    fn get_bit_stream_args_valid_dovi_preserves_metadata() {
        let mut state = invalid_dovi_state("hevc", Some("smpte2084"));
        let video = state.video_stream.as_mut().unwrap();
        video.color_space = Some("bt2020nc".to_owned());
        video.color_primaries = Some("bt2020".to_owned());
        state.base_request.video_range_type = Some("DOVIWithEL".to_owned());
        let caps = all_bsf();
        assert!(!is_dovi_removed(&caps, &state));
        assert_eq!(
            bit_stream_args(&caps, &state, MediaStreamType::Video).as_deref(),
            Some("-bsf:v hevc_mp4toannexb")
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

    // ----- the RPU strip for a client that did not list DOVI -----------------

    const HEVC_FIXUP: &str = "-bsf:v hevc_mp4toannexb";
    const HEVC_STRIP: &str = "-bsf:v hevc_mp4toannexb,hevc_metadata=remove_dovi=1";
    const AV1_STRIP: &str = "-bsf:v av1_metadata=remove_dovi=1";

    /// Divergence (owner decision 2026-10-31, gl-737): a Dolby Vision stream
    /// copied to a client that declared range types without `DOVI` or the
    /// stream's own range type has its RPU stripped. Upstream plans no removal
    /// for most of these rows (the base-layer fallbacks, HDR10+ beside Dolby
    /// Vision, an invalid configuration) and copies the RPU through. The rows
    /// a Dolby Vision player, a listed range type or upstream's own plan
    /// decides are upstream's.
    #[rstest]
    #[case::p8_1_hevc(
        "hevc",
        VideoRangeType::DoviWithHdr10,
        "SDR,HDR10,HLG",
        Some(HEVC_STRIP)
    )]
    #[case::p8_1_av1(
        "av1",
        VideoRangeType::DoviWithHdr10,
        "SDR,HDR10,HDR10Plus,HLG",
        Some(AV1_STRIP)
    )]
    #[case::p8_4("hevc", VideoRangeType::DoviWithHlg, "SDR,HLG", Some(HEVC_STRIP))]
    #[case::p8_2("hevc", VideoRangeType::DoviWithSdr, "SDR,HDR10", Some(HEVC_STRIP))]
    #[case::p8_1_hdr10_plus(
        "hevc",
        VideoRangeType::DoviWithHdr10Plus,
        "SDR,HDR10,HDR10Plus",
        Some(HEVC_STRIP)
    )]
    #[case::p7_to_hlg("hevc", VideoRangeType::DoviWithEl, "SDR,HLG", Some(HEVC_STRIP))]
    #[case::invalid("av1", VideoRangeType::DoviInvalid, "SDR,HDR10", Some(AV1_STRIP))]
    // Upstream's own: a Dolby Vision player keeps the RPU...
    #[case::dovi_player(
        "hevc",
        VideoRangeType::DoviWithHdr10,
        "SDR,HDR10,DOVI",
        Some(HEVC_FIXUP)
    )]
    #[case::dovi_player_av1("av1", VideoRangeType::DoviWithHdr10, "HDR10,DOVI", None)]
    // ...so does a client that lists the stream's range type...
    #[case::listed(
        "hevc",
        VideoRangeType::DoviWithHdr10,
        "HDR10,DOVIWithHDR10",
        Some(HEVC_FIXUP)
    )]
    // ...and one that declares no ranges at all.
    #[case::no_ranges("hevc", VideoRangeType::DoviWithHdr10, "", Some(HEVC_FIXUP))]
    // Profile 5 has no base layer the strip would leave playable.
    #[case::p5("hevc", VideoRangeType::Dovi, "SDR,HDR10", Some(HEVC_FIXUP))]
    // An enhancement-layer player loses the HDR10+ instead, as upstream.
    #[case::el_player(
        "hevc",
        VideoRangeType::DoviWithElhdr10Plus,
        "HDR10,DOVIWithEL",
        Some("-bsf:v hevc_mp4toannexb,hevc_metadata=remove_hdr10plus=1")
    )]
    fn a_client_that_did_not_list_dovi_gets_the_rpu_stripped(
        #[case] codec: &str,
        #[case] range_type: VideoRangeType,
        #[case] requested: &str,
        #[case] expected: Option<&str>,
    ) {
        let requested: Vec<&str> = requested.split(',').filter(|r| !r.is_empty()).collect();
        let state = state_with(video(codec, range_type), &requested);
        let caps = all_bsf();
        assert_eq!(
            bit_stream_args(&caps, &state, MediaStreamType::Video).as_deref(),
            expected,
            "{codec}/{range_type:?}/{requested:?}"
        );
        assert_eq!(
            is_dovi_removed(&caps, &state),
            expected.is_some_and(|args| args.contains("remove_dovi")),
            "{codec}/{range_type:?}/{requested:?}"
        );
    }

    #[test]
    fn the_strip_uses_the_generic_filter_where_the_codec_one_is_missing() {
        let caps = caps_with(&[BsfOption::DoviRpuStrip]);
        for (codec, expected) in [
            ("hevc", "-bsf:v hevc_mp4toannexb,dovi_rpu=strip=1"),
            ("av1", "-bsf:v dovi_rpu=strip=1"),
        ] {
            let state = state_with(video(codec, VideoRangeType::DoviWithHdr10), &["HDR10"]);
            assert_eq!(
                bit_stream_args(&caps, &state, MediaStreamType::Video).as_deref(),
                Some(expected),
                "{codec}"
            );
            assert!(is_dovi_removed(&caps, &state), "{codec}");
        }
    }

    #[test]
    fn an_ffmpeg_that_cannot_strip_gets_upstreams_arguments() {
        // The copy check transcodes such a stream. A copy it cannot refuse (the
        // client asked for `copy`, or may not transcode) gets what upstream
        // sends rather than a filter this ffmpeg does not have.
        let caps = caps_with(&[]);
        let fallback = state_with(video("hevc", VideoRangeType::DoviWithHdr10), &["HDR10"]);
        assert_eq!(
            bit_stream_args(&caps, &fallback, MediaStreamType::Video).as_deref(),
            Some(HEVC_FIXUP)
        );
        assert!(!is_dovi_removed(&caps, &fallback));
        // Upstream's own removal plan is untouched.
        let el = state_with(video("hevc", VideoRangeType::DoviWithEl), &["HDR10"]);
        assert_eq!(
            bit_stream_args(&caps, &el, MediaStreamType::Video).as_deref(),
            Some("-bsf:v hevc_mp4toannexb,dovi_rpu=strip=1")
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

    // ----- GetAudioBitStreamArguments (EncodingHelperAudioBitStreamTests) -----

    /// `DefaultSeekTicks` from upstream's test — 63.063 seconds.
    const DEFAULT_SEEK_TICKS: i64 = 630_630_000;

    const BOTH_FILTERS: &str = "-bsf:a noise=drop='lt(pts*tb\\,63.063)',aac_adtstoasc";
    const NOISE_ONLY: &str = "-bsf:a noise=drop='lt(pts*tb\\,63.063)'";
    const ADTS_ONLY: &str = "-bsf:a aac_adtstoasc";

    fn audio_bsf_state(
        job_type: ferrofin_traits::media_encoding::TranscodingJobType,
        output_video_codec: &str,
        output_audio_codec: &str,
        audio_stream_codec: &str,
        input_container: &str,
        start_ticks: i64,
    ) -> EncodingJobInfo {
        EncodingJobInfo {
            display: crate::encoding_helper::transcode_state::TranscodeDisplayNames::default(),
            base_request: crate::BaseEncodingJobOptions {
                start_time_ticks: Some(start_ticks),
                ..crate::BaseEncodingJobOptions::default()
            },
            video_stream: Some(MediaStream {
                codec: Some("h264".to_owned()),
                index: 0,
                stream_type: MediaStreamType::Video,
                ..MediaStream::default()
            }),
            audio_stream: Some(MediaStream {
                codec: Some(audio_stream_codec.to_owned()),
                index: 1,
                stream_type: MediaStreamType::Audio,
                ..MediaStream::default()
            }),
            subtitle_stream: None,
            media_source: ferrofin_model::dto::MediaSourceInfo::default(),
            output_video_codec: Some(output_video_codec.to_owned()),
            output_audio_codec: Some(output_audio_codec.to_owned()),
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: None,
            output_container: None,
            output_video_sync: None,
            output_file_path: String::new(),
            input_container: Some(input_container.to_owned()),
            is_input_video: true,
            subtitle_delivery_method: ferrofin_model::dlna::enums::SubtitleDeliveryMethod::Encode,
            trimmed_subtitle_path: None,
            transcode_reasons: ferrofin_model::session::TranscodeReasons::empty(),
            run_time_ticks: Some(6_000_000_000),
            transcoding_type: job_type,
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

    /// Port of `EncodingHelperAudioBitStreamTests.AudioBitStreamArguments_AppliesGates`.
    /// Upstream passes `mediaSourceContainer` separately from
    /// `state.InputContainer` (in production they are the same value), and
    /// its timeline is the source's (`-copyts`): origin 0.
    #[rstest]
    #[case::hls_aac_ts_to_mp4(TranscodingJobType::Hls, ("libx264", "copy", "aac", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "ts"), BOTH_FILTERS)]
    #[case::hls_aac_aac_to_mp4(TranscodingJobType::Hls, ("libx264", "copy", "aac", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "aac"), BOTH_FILTERS)]
    #[case::hls_aac_hls_to_mp4(TranscodingJobType::Hls, ("libx264", "copy", "aac", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "hls"), BOTH_FILTERS)]
    #[case::progressive(TranscodingJobType::Progressive, ("libx264", "copy", "aac", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "ts"), ADTS_ONLY)]
    #[case::video_copy(TranscodingJobType::Hls, ("copy", "copy", "aac", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "ts"), ADTS_ONLY)]
    #[case::audio_not_copy(TranscodingJobType::Hls, ("libx264", "aac", "aac", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "ts"), ADTS_ONLY)]
    #[case::wtv_input(TranscodingJobType::Hls, ("libx264", "copy", "aac", "wtv"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "ts"), ADTS_ONLY)]
    #[case::zero_seek(TranscodingJobType::Hls, ("libx264", "copy", "aac", "ts"), 0, (5, 0), ("mp4", "ts"), ADTS_ONLY)]
    #[case::old_ffmpeg(TranscodingJobType::Hls, ("libx264", "copy", "aac", "ts"), DEFAULT_SEEK_TICKS, (4, 4), ("mp4", "ts"), ADTS_ONLY)]
    #[case::ts_segment(TranscodingJobType::Hls, ("libx264", "copy", "aac", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("ts", "ts"), NOISE_ONLY)]
    #[case::mkv_source(TranscodingJobType::Hls, ("libx264", "copy", "aac", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "mkv"), NOISE_ONLY)]
    #[case::ac3_audio(TranscodingJobType::Hls, ("libx264", "copy", "ac3", "ts"), DEFAULT_SEEK_TICKS, (5, 0), ("mp4", "ts"), NOISE_ONLY)]
    fn audio_bit_stream_arguments_applies_gates(
        #[case] job_type: TranscodingJobType,
        #[case] codecs: (&str, &str, &str, &str),
        #[case] start_ticks: i64,
        #[case] ffmpeg_ver: (i32, i32),
        #[case] containers: (&str, &str),
        #[case] expected: &str,
    ) {
        let (output_video_codec, output_audio_codec, audio_stream_codec, input_container) = codecs;
        let (segment_container, media_source_container) = containers;
        let state = audio_bsf_state(
            job_type,
            output_video_codec,
            output_audio_codec,
            audio_stream_codec,
            input_container,
            start_ticks,
        );
        let version = Some(FfmpegVersion::new(ffmpeg_ver.0, ffmpeg_ver.1));
        assert_eq!(
            audio_bit_stream_arguments(
                &state,
                segment_container,
                media_source_container,
                0,
                version
            )
            .as_deref(),
            Some(expected),
        );
    }

    /// No audio stream suppresses the adtstoasc fixup but not the noise
    /// trim (the caller guards the whole call behind `audio_stream.is_some()`
    /// anyway — upstream's `GetAudioArguments` does the same null check).
    #[test]
    fn audio_bsf_without_audio_stream_still_emits_noise_trim() {
        let mut state = audio_bsf_state(
            ferrofin_traits::media_encoding::TranscodingJobType::Hls,
            "libx264",
            "copy",
            "aac",
            "ts",
            DEFAULT_SEEK_TICKS,
        );
        state.audio_stream = None;
        assert_eq!(
            audio_bit_stream_arguments(&state, "mp4", "ts", 0, Some(FfmpegVersion::new(5, 0)),)
                .as_deref(),
            Some(NOISE_ONLY),
        );
    }

    /// Without `-copyts` the packets count from the input seek, so the cut
    /// is the seek less that origin: 0 at the seek itself, the rest when the
    /// seek was clamped earlier (gl-1018).
    #[rstest]
    #[case::at_the_seek(DEFAULT_SEEK_TICKS, "0.000")]
    #[case::clamped_earlier(DEFAULT_SEEK_TICKS - 30_000_000, "3.000")]
    fn copied_audio_trim_counts_from_the_timeline_origin(
        #[case] origin_ticks: i64,
        #[case] cutoff: &str,
    ) {
        let state = audio_bsf_state(
            TranscodingJobType::Hls,
            "libx264",
            "copy",
            "ac3",
            "mkv",
            DEFAULT_SEEK_TICKS,
        );
        assert_eq!(
            audio_bit_stream_arguments(
                &state,
                "mp4",
                "mkv",
                origin_ticks,
                Some(FfmpegVersion::new(5, 0))
            ),
            Some(format!("-bsf:a noise=drop='lt(pts*tb\\,{cutoff})'")),
        );
    }

    /// A progressive job embedding the subtitle `sub` (`None`: no subtitle).
    fn embed_state(sub: Option<MediaStream>, method: SubtitleDeliveryMethod) -> EncodingJobInfo {
        EncodingJobInfo {
            subtitle_stream: sub,
            subtitle_delivery_method: method,
            ..audio_bsf_state(
                TranscodingJobType::Progressive,
                "libx264",
                "aac",
                "aac",
                "mkv",
                DEFAULT_SEEK_TICKS,
            )
        }
    }

    /// A subtitle stream, external (a sidecar) or inside the media file.
    fn subtitle(external: bool) -> MediaStream {
        MediaStream {
            codec: Some("subrip".to_owned()),
            index: 2,
            stream_type: MediaStreamType::Subtitle,
            is_external: external,
            path: external.then(|| "/media/movie.en.srt".to_owned()),
            ..MediaStream::default()
        }
    }

    const FILTER_EVENTS: &str = "-bsf:s:0 noise=drop='lt(pts\\,0)*lte(pts+duration\\,0)',setts=pts='max(PTS\\,0)':dts='max(DTS\\,0)':duration='DURATION+min(PTS\\,0)'";
    const NOISE_DROP: &str = "-bsf:s:0 noise=drop='lt(pts\\,0)'";
    const COPY_PRIOR: &str = "-copypriorss:s:0 0";

    /// An embedded sidecar in a seeked stream is trimmed to the output's start
    /// (gl-930): from ffmpeg 5.1 as `FilterEvents` trims it (elapsed cues
    /// dropped, the one on screen started at 0), on 5.0 every cue before the
    /// start dropped by the `noise` filter, `-copypriorss` on an older or
    /// unread version. Nothing for an unseeked stream, a track in the media
    /// file, another delivery method or no subtitle.
    #[rstest]
    #[case::from_5_1(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((5, 1)), Some(FILTER_EVENTS))]
    #[case::current(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((9, 0)), Some(FILTER_EVENTS))]
    #[case::on_5_0(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((5, 0)), Some(NOISE_DROP))]
    #[case::before_5_0(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((4, 4)), Some(COPY_PRIOR))]
    #[case::unknown_version(
        true,
        Some(subtitle(true)),
        SubtitleDeliveryMethod::Embed,
        None,
        Some(COPY_PRIOR)
    )]
    #[case::not_seeked(false, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((9, 0)), None)]
    #[case::internal_track(true, Some(subtitle(false)), SubtitleDeliveryMethod::Embed, Some((9, 0)), None)]
    #[case::burned_in(true, Some(subtitle(true)), SubtitleDeliveryMethod::Encode, Some((9, 0)), None)]
    #[case::external_delivery(true, Some(subtitle(true)), SubtitleDeliveryMethod::External, Some((9, 0)), None)]
    #[case::no_subtitle(true, None, SubtitleDeliveryMethod::Embed, Some((9, 0)), None)]
    fn an_embedded_sidecar_drops_the_cues_before_a_seek(
        #[case] seeked: bool,
        #[case] sub: Option<MediaStream>,
        #[case] method: SubtitleDeliveryMethod,
        #[case] version: Option<(i32, i32)>,
        #[case] expected: Option<&str>,
    ) {
        let state = embed_state(sub, method);
        let version = version.map(|(major, minor)| FfmpegVersion::new(major, minor));
        assert_eq!(
            embedded_sidecar_trim_arguments(&state, seeked, version),
            expected
        );
    }

    /// Copied timestamps keep the source's clock, so nothing is dropped; they
    /// count only with a known runtime, as the planner's `-copyts` does.
    #[rstest]
    #[case::copied(Some(6_000_000_000), None)]
    #[case::unknown_runtime(None, Some(FILTER_EVENTS))]
    fn copied_timestamps_keep_the_sidecar_cues(
        #[case] run_time_ticks: Option<i64>,
        #[case] expected: Option<&str>,
    ) {
        let mut state = embed_state(Some(subtitle(true)), SubtitleDeliveryMethod::Embed);
        state.base_request.copy_timestamps = true;
        state.run_time_ticks = run_time_ticks;
        assert_eq!(
            embedded_sidecar_trim_arguments(&state, true, Some(FfmpegVersion::new(9, 0))),
            expected
        );
    }

    /// An external `.sup` sidecar.
    fn bitmap_sidecar() -> MediaStream {
        MediaStream {
            codec: Some("PGSSUB".to_owned()),
            path: Some("/media/movie.en.sup".to_owned()),
            ..subtitle(true)
        }
    }

    /// Before ffmpeg 5.1, or with a version that cannot be read, a seeked
    /// stream embeds a text sidecar from a copy the server cut to the start
    /// (gl-1199): the output trim there drops the cue on screen at the start,
    /// and before 5.0 shifts every stream for a converted track. Not from 5.1,
    /// whose trim cuts as `FilterEvents` does; not without a seek, for a track
    /// in the media file, a bitmap sidecar, another delivery method, or a job
    /// that copies timestamps.
    #[rstest]
    #[case::on_5_0(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((5, 0)), true)]
    #[case::before_5_0(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((4, 4)), true)]
    #[case::unknown_version(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, None, true)]
    #[case::from_5_1(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((5, 1)), false)]
    #[case::current(true, Some(subtitle(true)), SubtitleDeliveryMethod::Embed, Some((9, 0)), false)]
    #[case::not_seeked(
        false,
        Some(subtitle(true)),
        SubtitleDeliveryMethod::Embed,
        None,
        false
    )]
    #[case::internal_track(
        true,
        Some(subtitle(false)),
        SubtitleDeliveryMethod::Embed,
        None,
        false
    )]
    #[case::bitmap(
        true,
        Some(bitmap_sidecar()),
        SubtitleDeliveryMethod::Embed,
        None,
        false
    )]
    #[case::burned_in(
        true,
        Some(subtitle(true)),
        SubtitleDeliveryMethod::Encode,
        None,
        false
    )]
    #[case::no_subtitle(true, None, SubtitleDeliveryMethod::Embed, None, false)]
    fn a_seeked_text_sidecar_is_cut_by_the_server_before_ffmpeg_5_1(
        #[case] seeked: bool,
        #[case] sub: Option<MediaStream>,
        #[case] method: SubtitleDeliveryMethod,
        #[case] version: Option<(i32, i32)>,
        #[case] expected: bool,
    ) {
        let state = embed_state(sub, method);
        let version = version.map(|(major, minor)| FfmpegVersion::new(major, minor));
        assert_eq!(
            embedded_sidecar_needs_trimmed_copy(&state, seeked, version),
            expected
        );
    }

    /// Nor does a job copying timestamps get the copy: its clock keeps the
    /// cues before the start positive.
    #[test]
    fn copied_timestamps_read_the_sidecar_itself() {
        let mut state = embed_state(Some(subtitle(true)), SubtitleDeliveryMethod::Embed);
        state.base_request.copy_timestamps = true;
        state.run_time_ticks = Some(6_000_000_000);
        assert!(!embedded_sidecar_needs_trimmed_copy(&state, true, None));
    }

    /// The copy the server cut starts at the start, so the output trim adds
    /// nothing for it.
    #[rstest]
    #[case::on_5_0(Some((5, 0)))]
    #[case::before_5_0(Some((4, 4)))]
    #[case::unknown_version(None)]
    fn a_trimmed_copy_needs_no_output_trim(#[case] version: Option<(i32, i32)>) {
        let mut state = embed_state(Some(subtitle(true)), SubtitleDeliveryMethod::Embed);
        state.trimmed_subtitle_path = Some("/cache/transcodes/abc-stream.90000000.srt".into());
        let version = version.map(|(major, minor)| FfmpegVersion::new(major, minor));
        assert_eq!(embedded_sidecar_trim_arguments(&state, true, version), None);
    }
}
