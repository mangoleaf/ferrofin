//! The shared filter fragments every hardware chain is built from.
//!
//! Port of C# `EncodingHelper`'s filter helpers (10.11.z lines 3240–3374 and
//! 3534–3599, plus `GetVideoTransposeDirection` at 3743–3761): the hardware
//! scaler, the graphical-subtitle pre-process chain, the `alphasrc` source that
//! makes text subtitles overlayable on a GPU, the software and hardware
//! deinterlacers, and the rotation direction.
//!
//! These are the pieces; the per-vendor chains that assemble them into a
//! filtergraph are the work items of later phases. Each returns a bare filter
//! string with **no** leading space — unlike the device arguments, filters are
//! joined with commas by the chain builder, not concatenated.

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::entities::DeinterlaceMethod;

use super::capabilities::FfmpegCapabilities;
use super::decoder::{RequestedSize, fixed_output_size, is_size_fixed};

/// The frame rate `alphasrc` runs at when the real one is unknown. Port of the
/// `framerate ?? 25` fallback in `GetAlphaSrcFilter`.
pub const DEFAULT_ALPHASRC_FRAMERATE: f32 = 25.0;

/// The deinterlacer's frame-rate ceiling for double-rate output.
///
/// Port of the `ReferenceFrameRate <= 30` test: doubling a 60fps source would
/// ask the encoder for 120fps, which no client expects and few can play.
pub const MAX_DOUBLE_RATE_SOURCE_FPS: f32 = 30.0;

/// How close two aspect ratios must be before a subtitle is simply rescaled
/// rather than padded and cropped.
///
/// Port of the `0.01f` in `GetGraphicalSubPreProcessFilters`. It is a **float**
/// literal in the C#, which widens to `0.009999999776…` rather than to the
/// nearer double — and the two differ over a band that real 1000-pixel-wide
/// sources land in, where the choice is between two entirely different filter
/// chains. Declared at `f32` and widened the same way so the boundary matches.
pub const SUBTITLE_DAR_EPSILON: f32 = 0.01;

/// A hardware scale filter, or an empty string when there is nothing to do.
///
/// Port of `GetHwScaleFilter`. The filter is emitted only if the size or the
/// pixel format actually changes — a `scale_vaapi` that changes neither still
/// costs a full pass over every frame.
///
/// `prefix`/`suffix` name the filter (`scale`+`vaapi`, `vpp`+`qsv`,
/// `scale`+`cuda`, …). `swap_output_dimensions` is set when a transpose later
/// in the chain will rotate the frame, so the scaler has to produce the
/// pre-rotation shape.
#[must_use]
pub fn hw_scale_filter(
    prefix: &str,
    suffix: &str,
    video_format: Option<&str>,
    swap_output_dimensions: bool,
    video_width: Option<i32>,
    video_height: Option<i32>,
    requested: RequestedSize,
) -> String {
    let (out_width, out_height) = fixed_output_size(video_width, video_height, requested);
    let (Some(out_width), Some(out_height)) = (out_width, out_height) else {
        return String::new();
    };

    let is_format_fixed = video_format.is_some_and(|f| !f.is_empty());
    let is_size_fixed = is_size_fixed(video_width, video_height, out_width, out_height);

    if suffix.is_empty() || !(is_size_fixed || is_format_fixed) {
        return String::new();
    }

    let (w, h) = if swap_output_dimensions {
        (out_height, out_width)
    } else {
        (out_width, out_height)
    };
    let size_arg = if is_size_fixed {
        format!("=w={w}:h={h}")
    } else {
        String::new()
    };
    let format_arg = match video_format.filter(|f| !f.is_empty()) {
        // The separator depends on whether a size argument already opened the
        // filter's option list.
        Some(format) => format!("{}format={format}", if is_size_fixed { ':' } else { '=' }),
        None => String::new(),
    };
    let prefix = if prefix.is_empty() { "scale" } else { prefix };
    format!("{prefix}_{suffix}{size_arg}{format_arg}")
}

/// The filter chain that fits a bitmap subtitle to the output frame. Port of
/// `GetGraphicalSubPreProcessFilters`.
///
/// A subtitle bitmap is authored for one frame size and has to land on another.
/// When its aspect ratio already matches the video's, a plain rescale is
/// enough; otherwise it is scaled to height, then padded and cropped so it sits
/// centred on a transparent canvas of exactly the output size — which is what
/// keeps 4:3 subtitles from stretching across a 16:9 frame.
#[must_use]
pub fn graphical_sub_preprocess_filters(
    video_width: Option<i32>,
    video_height: Option<i32>,
    subtitle_width: Option<i32>,
    subtitle_height: Option<i32>,
    requested: RequestedSize,
) -> String {
    let (out_width, out_height) = fixed_output_size(video_width, video_height, requested);
    let (Some(w), Some(h)) = (out_width, out_height) else {
        return String::new();
    };
    if w <= 0 || h <= 0 {
        return String::new();
    }

    if let (Some(sub_w), Some(sub_h)) = (subtitle_width, subtitle_height)
        && sub_w > 0
        && sub_h > 0
    {
        let video_dar = f64::from(w) / f64::from(h);
        let subtitle_dar = f64::from(sub_w) / f64::from(sub_h);
        // Same shape (1080p subtitles on a 2160p video, say): no padding needed.
        if (video_dar - subtitle_dar).abs() < f64::from(SUBTITLE_DAR_EPSILON) {
            return format!("scale,scale={w}:{h}:fast_bilinear");
        }
    }

    format!(
        "scale,scale=-1:{h}:fast_bilinear,crop,pad=max({w}\\,iw):max({h}\\,ih):\
         (ow-iw)/2:(oh-ih)/2:black@0,crop={w}:{h}"
    )
}

/// The transparent video source a text subtitle is rendered onto before it can
/// be overlaid on the GPU. Port of `GetAlphaSrcFilter`.
///
/// Hardware overlay filters take two *video* inputs, so a text subtitle — which
/// is not video — has to become one first. `alphasrc` generates the empty
/// transparent frames the `subtitles` filter then draws into.
///
/// `start_time_ticks` is the seek position: the generated source must start
/// where the output does, or every subtitle is offset by the seek amount.
#[must_use]
pub fn alpha_src_filter(
    video_width: Option<i32>,
    video_height: Option<i32>,
    requested: RequestedSize,
    framerate: Option<f32>,
    start_time_ticks: i64,
) -> String {
    let (out_width, out_height) = fixed_output_size(video_width, video_height, requested);
    let (Some(w), Some(h)) = (out_width, out_height) else {
        return String::new();
    };
    let rate = framerate.unwrap_or(DEFAULT_ALPHASRC_FRAMERATE);
    // Upstream emits a bare `0` rather than a formatted timestamp when there is
    // no seek, and the quotes are part of the filter syntax either way.
    let start = if start_time_ticks > 0 {
        format_ticks_as_timestamp(start_time_ticks)
    } else {
        "0".to_owned()
    };
    format!("alphasrc=s={w}x{h}:r={rate}:start='{start}'")
}

/// Formats .NET ticks (100-nanosecond units) as ffmpeg's escaped
/// `hh\:mm\:ss\.fff`.
///
/// Port of `TimeSpan.FromTicks(reqTicks).ToString(@"hh\\\:mm\\\:ss\\\.fff")`.
/// That format string is *verbatim*, so under .NET's custom-TimeSpan escape
/// rules each `\\` becomes a literal backslash and each `\:` a literal colon —
/// the rendered separators are `\:` and `\.`, backslashes included.
///
/// The backslashes are load-bearing, not decoration. A filter argument is
/// itself colon-separated, so an unescaped `00:00:01.000` splits into three
/// options and ffmpeg rejects the filtergraph outright with
/// `Error opening input: Invalid argument`; the single quotes around the value
/// do not protect it. Verified against ffmpeg n9.0.1 both ways.
fn format_ticks_as_timestamp(ticks: i64) -> String {
    const TICKS_PER_MILLISECOND: i64 = 10_000;
    let total_ms = ticks / TICKS_PER_MILLISECOND;
    let ms = total_ms % 1000;
    let total_seconds = total_ms / 1000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    // .NET's `hh` renders hours-within-the-day and silently drops whole days;
    // this keeps them, which is both more correct and unreachable for real
    // media (a seek past 24 hours).
    let hours = total_seconds / 3600;
    format!("{hours:02}\\:{minutes:02}\\:{seconds:02}\\.{ms:03}")
}

/// The software deinterlacer. Port of `GetSwDeinterlaceFilter`.
#[must_use]
pub fn sw_deinterlace_filter(
    options: &EncodingOptions,
    reference_frame_rate: Option<f32>,
) -> String {
    let method = format!("{:?}", options.deinterlace_method).to_ascii_lowercase();
    format!(
        "{method}={}:-1:0",
        double_rate_flag(options, reference_frame_rate)
    )
}

/// The hardware deinterlacer for a backend suffix, or an empty string when that
/// backend has none. Port of `GetHwDeinterlaceFilter`.
///
/// The backends do not agree on how to spell this. CUDA, OpenCL and
/// VideoToolbox take a `yadif`/`bwdif` variant with the same argument shape as
/// software; VAAPI takes a rate *mode*; QSV takes a fixed `mode=2` and no rate
/// choice at all.
#[must_use]
pub fn hw_deinterlace_filter(
    caps: &FfmpegCapabilities,
    options: &EncodingOptions,
    reference_frame_rate: Option<f32>,
    hw_deint_suffix: &str,
) -> String {
    let double_rate = double_rate_flag(options, reference_frame_rate);
    let wants_bwdif = options.deinterlace_method == DeinterlaceMethod::bwdif;
    let suffix = hw_deint_suffix.to_ascii_lowercase();

    if suffix.contains("cuda") {
        let filter = if wants_bwdif && caps.supports_filter("bwdif_cuda") {
            "bwdif"
        } else {
            "yadif"
        };
        return format!("{filter}_cuda={double_rate}:-1:0");
    }

    if suffix.contains("opencl") {
        // Upstream requires BOTH to be present before using either, so a build
        // with only one falls back to software deinterlacing entirely.
        if caps.supports_filter("yadif_opencl") && caps.supports_filter("bwdif_opencl") {
            let filter = if wants_bwdif { "bwdif" } else { "yadif" };
            return format!("{filter}_opencl={double_rate}:-1:0");
        }
        return String::new();
    }

    if suffix.contains("vaapi") {
        // VAAPI names the rate rather than flagging it.
        let rate = if double_rate == "1" { "field" } else { "frame" };
        return format!("deinterlace_vaapi=rate={rate}");
    }

    if suffix.contains("qsv") {
        // `mode=2` is the advanced deinterlacer; QSV offers no rate choice.
        return "deinterlace_qsv=mode=2".to_owned();
    }

    if suffix.contains("videotoolbox") {
        let filter = if wants_bwdif && caps.supports_filter("bwdif_videotoolbox") {
            "bwdif"
        } else {
            "yadif"
        };
        return format!("{filter}_videotoolbox={double_rate}:-1:0");
    }

    String::new()
}

/// `"1"` for double-rate deinterlacing, `"0"` otherwise.
///
/// An **unknown** frame rate is not eligible. The two C# call sites spell that
/// differently — the software one relies on `null <= 30` being false under
/// lifted comparison, the hardware one writes `?? 60` explicitly — but they
/// reach the same answer, so one helper serves both.
fn double_rate_flag(options: &EncodingOptions, reference_frame_rate: Option<f32>) -> &'static str {
    if options.deinterlace_double_rate
        && reference_frame_rate.is_some_and(|rate| rate <= MAX_DOUBLE_RATE_SOURCE_FPS)
    {
        "1"
    } else {
        "0"
    }
}

/// The `transpose` direction for a rotated source, or an empty string when the
/// frame is upright. Port of `GetVideoTransposeDirection`.
#[must_use]
pub fn video_transpose_direction(rotation: Option<i32>) -> &'static str {
    match rotation.unwrap_or(0) {
        90 => "cclock",
        180 | -180 => "reversal",
        -90 => "clock",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // Hand-derived from the C# (10.11.z 3240-3374, 3534-3599, 3743-3761).
    // Upstream ships no tests for any of it.

    fn caps_with_all_filters() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .filters(crate::encoder::REQUIRED_FILTERS)
            .build()
    }

    fn size(w: Option<i32>, h: Option<i32>) -> RequestedSize {
        RequestedSize {
            width: w,
            height: h,
            ..RequestedSize::default()
        }
    }

    // ----- the hardware scaler -----------------------------------------------

    #[test]
    fn a_scaler_that_would_change_nothing_is_not_emitted() {
        // Same size in and out, no format change: the filter would cost a full
        // pass over every frame for nothing.
        assert_eq!(
            hw_scale_filter(
                "scale",
                "vaapi",
                None,
                false,
                Some(1920),
                Some(1080),
                size(Some(1920), Some(1080))
            ),
            ""
        );
        // An empty suffix is not a filter name.
        assert_eq!(
            hw_scale_filter(
                "scale",
                "",
                Some("nv12"),
                false,
                Some(1920),
                Some(1080),
                size(Some(1280), Some(720))
            ),
            ""
        );
    }

    #[rstest]
    // The four filter spellings the vendors use.
    #[case("scale", "vaapi", "scale_vaapi=w=1280:h=720")]
    #[case("scale", "cuda", "scale_cuda=w=1280:h=720")]
    #[case("vpp", "qsv", "vpp_qsv=w=1280:h=720")]
    #[case("scale", "vt", "scale_vt=w=1280:h=720")]
    #[case("vpp", "rkrga", "vpp_rkrga=w=1280:h=720")]
    // An empty prefix defaults to `scale`.
    #[case("", "opencl", "scale_opencl=w=1280:h=720")]
    fn the_scaler_names_follow_the_vendor(
        #[case] prefix: &str,
        #[case] suffix: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(
            hw_scale_filter(
                prefix,
                suffix,
                None,
                false,
                Some(1920),
                Some(1080),
                size(Some(1280), Some(720))
            ),
            expected
        );
    }

    #[test]
    fn the_format_separator_depends_on_whether_a_size_was_written() {
        // Size AND format: the format joins the existing option list with `:`.
        assert_eq!(
            hw_scale_filter(
                "scale",
                "vaapi",
                Some("nv12"),
                false,
                Some(1920),
                Some(1080),
                size(Some(1280), Some(720))
            ),
            "scale_vaapi=w=1280:h=720:format=nv12"
        );
        // Format only: the format OPENS the option list with `=`.
        assert_eq!(
            hw_scale_filter(
                "scale",
                "vaapi",
                Some("nv12"),
                false,
                Some(1920),
                Some(1080),
                size(Some(1920), Some(1080))
            ),
            "scale_vaapi=format=nv12"
        );
    }

    #[test]
    fn either_dimension_changing_is_enough_to_emit_the_scaler() {
        // "Fixed" is per-dimension: a letterbox crop that keeps the width but
        // changes the height still needs the filter.
        assert_eq!(
            hw_scale_filter(
                "scale",
                "vaapi",
                None,
                false,
                Some(1920),
                Some(1080),
                size(Some(1920), Some(800))
            ),
            "scale_vaapi=w=1920:h=800"
        );
        assert_eq!(
            hw_scale_filter(
                "scale",
                "vaapi",
                None,
                false,
                Some(1920),
                Some(1080),
                size(Some(1280), Some(1080))
            ),
            "scale_vaapi=w=1280:h=1080"
        );
    }

    #[test]
    fn a_pending_transpose_swaps_the_scalers_output_dimensions() {
        // The scaler runs before the rotation, so it has to produce the
        // pre-rotation shape.
        assert_eq!(
            hw_scale_filter(
                "scale",
                "cuda",
                None,
                true,
                Some(1920),
                Some(1080),
                size(Some(1280), Some(720))
            ),
            "scale_cuda=w=720:h=1280"
        );
    }

    #[test]
    fn an_unknown_source_size_still_scales_to_the_request() {
        // "Fixed" includes "the input size is unknown", so the filter is
        // emitted rather than skipped.
        assert_eq!(
            hw_scale_filter(
                "scale",
                "vaapi",
                None,
                false,
                None,
                None,
                size(Some(1280), Some(720))
            ),
            "scale_vaapi=w=1280:h=720"
        );
    }

    // ----- graphical subtitle pre-processing ---------------------------------

    #[test]
    fn a_matching_aspect_ratio_needs_only_a_rescale() {
        // 1080p subtitles on a 2160p video: same shape, just bigger.
        assert_eq!(
            graphical_sub_preprocess_filters(
                Some(3840),
                Some(2160),
                Some(1920),
                Some(1080),
                RequestedSize::default()
            ),
            "scale,scale=3840:2160:fast_bilinear"
        );
    }

    #[test]
    fn a_different_aspect_ratio_is_padded_and_cropped_to_fit() {
        // 4:3 subtitles on a 16:9 frame must not stretch: scale to height, then
        // centre on a transparent canvas of the output size.
        assert_eq!(
            graphical_sub_preprocess_filters(
                Some(1920),
                Some(1080),
                Some(720),
                Some(576),
                RequestedSize::default()
            ),
            "scale,scale=-1:1080:fast_bilinear,crop,pad=max(1920\\,iw):max(1080\\,ih):\
             (ow-iw)/2:(oh-ih)/2:black@0,crop=1920:1080"
        );
    }

    #[test]
    fn the_aspect_ratio_threshold_switches_between_two_whole_chains() {
        // The epsilon decides between a plain rescale and the pad-and-crop
        // chain. 1000x500 video is DAR 2.0; a 1001x500 subtitle is 2.002 —
        // comfortably inside.
        let inside = graphical_sub_preprocess_filters(
            Some(1000),
            Some(500),
            Some(1001),
            Some(500),
            RequestedSize::default(),
        );
        assert_eq!(inside, "scale,scale=1000:500:fast_bilinear");

        // A 1005x500 subtitle is a DAR difference of 0.0099999999999998, which
        // is inside a double 0.01 but OUTSIDE C#'s float `0.01f`
        // (0.009999999776...). Pinning it proves the widening is right: with a
        // double literal this takes the rescale chain and diverges from
        // upstream.
        let on_the_boundary = graphical_sub_preprocess_filters(
            Some(1000),
            Some(500),
            Some(1005),
            Some(500),
            RequestedSize::default(),
        );
        assert!(
            on_the_boundary.contains("pad=max(1000\\,iw)"),
            "{on_the_boundary}"
        );

        // 1020x500 is DAR 2.04 — well outside, so the padding chain.
        let outside = graphical_sub_preprocess_filters(
            Some(1000),
            Some(500),
            Some(1020),
            Some(500),
            RequestedSize::default(),
        );
        assert!(outside.contains("pad=max(1000\\,iw)"), "{outside}");
    }

    #[test]
    fn unknown_subtitle_dimensions_take_the_padding_path() {
        // Without the subtitle's own shape there is nothing to compare, so the
        // safe (padding) chain is used.
        let filters = graphical_sub_preprocess_filters(
            Some(1920),
            Some(1080),
            None,
            None,
            RequestedSize::default(),
        );
        assert!(filters.contains("pad=max(1920\\,iw)"), "{filters}");
        // ...and an unknown OUTPUT size yields nothing at all.
        assert_eq!(
            graphical_sub_preprocess_filters(
                None,
                None,
                Some(720),
                Some(576),
                RequestedSize::default()
            ),
            ""
        );
    }

    // ----- alphasrc ----------------------------------------------------------

    #[test]
    fn alphasrc_generates_a_transparent_source_of_the_output_size() {
        assert_eq!(
            alpha_src_filter(
                Some(1920),
                Some(1080),
                RequestedSize::default(),
                Some(23.976),
                0
            ),
            "alphasrc=s=1920x1080:r=23.976:start='0'"
        );
        // An unknown frame rate falls back to 25.
        assert_eq!(
            alpha_src_filter(Some(1920), Some(1080), RequestedSize::default(), None, 0),
            "alphasrc=s=1920x1080:r=25:start='0'"
        );
        assert_eq!(
            alpha_src_filter(None, None, RequestedSize::default(), Some(25.0), 0),
            ""
        );
    }

    #[rstest]
    // .NET ticks are 100ns units. A seek must move alphasrc's start with it or
    // every subtitle lands offset by the seek amount — and the separators must
    // be BACKSLASH-escaped, because a filter argument is itself
    // colon-separated and ffmpeg rejects the whole filtergraph otherwise.
    #[case(10_000_000, r"00\:00\:01\.000")]
    #[case(600_000_000, r"00\:01\:00\.000")]
    #[case(36_000_000_000, r"01\:00\:00\.000")]
    #[case(12_345_000, r"00\:00\:01\.234")]
    #[case(37_230_123_000, r"01\:02\:03\.012")]
    fn alphasrc_carries_the_seek_position(#[case] ticks: i64, #[case] expected: &str) {
        assert_eq!(
            alpha_src_filter(
                Some(640),
                Some(480),
                RequestedSize::default(),
                Some(25.0),
                ticks
            ),
            format!("alphasrc=s=640x480:r=25:start='{expected}'")
        );
    }

    // ----- deinterlacing -----------------------------------------------------

    #[rstest]
    #[case(DeinterlaceMethod::yadif, false, "yadif=0:-1:0")]
    #[case(DeinterlaceMethod::bwdif, false, "bwdif=0:-1:0")]
    // Double rate is only offered up to 30fps: doubling 60fps would ask for
    // 120, which few clients can play.
    #[case(DeinterlaceMethod::yadif, true, "yadif=1:-1:0")]
    fn the_software_deinterlacer_takes_the_method_and_the_rate(
        #[case] method: DeinterlaceMethod,
        #[case] double_rate: bool,
        #[case] expected: &str,
    ) {
        let options = EncodingOptions {
            deinterlace_method: method,
            deinterlace_double_rate: double_rate,
            ..EncodingOptions::default()
        };
        assert_eq!(sw_deinterlace_filter(&options, Some(25.0)), expected);
    }

    #[test]
    fn double_rate_is_refused_above_thirty_frames_per_second() {
        let options = EncodingOptions {
            deinterlace_double_rate: true,
            ..EncodingOptions::default()
        };
        assert_eq!(sw_deinterlace_filter(&options, Some(25.0)), "yadif=1:-1:0");
        assert_eq!(sw_deinterlace_filter(&options, Some(30.0)), "yadif=1:-1:0");
        assert_eq!(sw_deinterlace_filter(&options, Some(50.0)), "yadif=0:-1:0");
        assert_eq!(sw_deinterlace_filter(&options, Some(60.0)), "yadif=0:-1:0");
        // An unknown source rate is NOT eligible — C# reaches that through
        // `null <= 30` being false on the software path and `?? 60` on the
        // hardware one, and both must agree.
        assert_eq!(sw_deinterlace_filter(&options, None), "yadif=0:-1:0");
        let caps = caps_with_all_filters();
        assert_eq!(
            hw_deinterlace_filter(&caps, &options, None, "cuda"),
            "yadif_cuda=0:-1:0"
        );
    }

    #[rstest]
    // Each backend spells this differently, and two of them do not take a rate.
    #[case("cuda", "yadif_cuda=0:-1:0")]
    #[case("opencl", "yadif_opencl=0:-1:0")]
    #[case("videotoolbox", "yadif_videotoolbox=0:-1:0")]
    #[case("vaapi", "deinterlace_vaapi=rate=frame")]
    #[case("qsv", "deinterlace_qsv=mode=2")]
    // A backend with no hardware deinterlacer.
    #[case("rkrga", "")]
    #[case("", "")]
    fn every_backend_spells_deinterlacing_differently(
        #[case] suffix: &str,
        #[case] expected: &str,
    ) {
        let caps = caps_with_all_filters();
        let options = EncodingOptions::default();
        assert_eq!(
            hw_deinterlace_filter(&caps, &options, Some(25.0), suffix),
            expected
        );
    }

    #[test]
    fn vaapi_names_the_rate_where_the_others_flag_it() {
        let caps = caps_with_all_filters();
        let options = EncodingOptions {
            deinterlace_double_rate: true,
            ..EncodingOptions::default()
        };
        assert_eq!(
            hw_deinterlace_filter(&caps, &options, Some(25.0), "vaapi"),
            "deinterlace_vaapi=rate=field"
        );
        assert_eq!(
            hw_deinterlace_filter(&caps, &options, Some(60.0), "vaapi"),
            "deinterlace_vaapi=rate=frame"
        );
        // QSV takes no rate at all, however it is configured.
        assert_eq!(
            hw_deinterlace_filter(&caps, &options, Some(25.0), "qsv"),
            "deinterlace_qsv=mode=2"
        );
    }

    #[test]
    fn bwdif_is_used_only_where_the_build_actually_has_it() {
        let options = EncodingOptions {
            deinterlace_method: DeinterlaceMethod::bwdif,
            ..EncodingOptions::default()
        };
        let all = caps_with_all_filters();
        assert_eq!(
            hw_deinterlace_filter(&all, &options, Some(25.0), "cuda"),
            "bwdif_cuda=0:-1:0"
        );

        // Without `bwdif_cuda` the request silently degrades to yadif rather
        // than dropping to software.
        let without = FfmpegCapabilities::builder()
            .filters(
                crate::encoder::REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != "bwdif_cuda"),
            )
            .build();
        assert_eq!(
            hw_deinterlace_filter(&without, &options, Some(25.0), "cuda"),
            "yadif_cuda=0:-1:0"
        );

        // VideoToolbox probes its own bwdif separately.
        assert_eq!(
            hw_deinterlace_filter(&all, &options, Some(25.0), "videotoolbox"),
            "bwdif_videotoolbox=0:-1:0"
        );
        let without_vt = FfmpegCapabilities::builder()
            .filters(
                crate::encoder::REQUIRED_FILTERS
                    .into_iter()
                    .filter(|f| *f != "bwdif_videotoolbox"),
            )
            .build();
        assert_eq!(
            hw_deinterlace_filter(&without_vt, &options, Some(25.0), "videotoolbox"),
            "yadif_videotoolbox=0:-1:0"
        );
    }

    #[test]
    fn opencl_deinterlacing_needs_both_filters_or_neither() {
        // Upstream requires yadif_opencl AND bwdif_opencl before using either,
        // so a build with only one deinterlaces in software instead.
        let options = EncodingOptions::default();
        for missing in ["yadif_opencl", "bwdif_opencl"] {
            let caps = FfmpegCapabilities::builder()
                .filters(
                    crate::encoder::REQUIRED_FILTERS
                        .into_iter()
                        .filter(|f| *f != missing),
                )
                .build();
            assert_eq!(
                hw_deinterlace_filter(&caps, &options, Some(25.0), "opencl"),
                "",
                "{missing} missing must disable OpenCL deinterlacing entirely"
            );
        }
    }

    // ----- rotation ----------------------------------------------------------

    #[rstest]
    #[case(Some(90), "cclock")]
    #[case(Some(-90), "clock")]
    #[case(Some(180), "reversal")]
    #[case(Some(-180), "reversal")]
    #[case(Some(0), "")]
    #[case(None, "")]
    // Anything that is not a right angle is not a transpose upstream handles.
    #[case(Some(45), "")]
    #[case(Some(270), "")]
    fn rotation_maps_to_a_transpose_direction(
        #[case] rotation: Option<i32>,
        #[case] expected: &str,
    ) {
        assert_eq!(video_transpose_direction(rotation), expected);
    }
}
