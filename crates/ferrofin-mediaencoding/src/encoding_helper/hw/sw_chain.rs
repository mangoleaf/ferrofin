//! The software filter chain, and the assembly that turns any chain into
//! ffmpeg arguments.
//!
//! Port of C# `EncodingHelper.GetSwVidFilterChain` (10.11.z 3762–3874),
//! `GetSwScaleFilter`/`GetFixedSwScaleFilter` (3375–3533), and the
//! `GetVideoProcessingFilterParam` dispatcher (6160–6265).
//!
//! Every vendor chain — software and hardware alike — produces the same shape:
//! **three** lists of filters, not one. `main` operates on the video, `sub`
//! prepares the subtitle, and `overlay` composites them. That split is what
//! decides the argument ffmpeg receives: a job with no subtitle overlay gets a
//! simple `-vf "…"`, while one with an overlay needs `-filter_complex` with
//! named pads, because two inputs have to meet.
//!
//! The software chain is also every hardware chain's fallback — each vendor
//! chain's first act is to check its own prerequisites and return this one if
//! they are missing — so its correctness matters far beyond software-only
//! installs.

use std::fmt::Write as _;

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::entities::{TonemappingRange, Video3DFormat};

use super::decoder::RequestedSize;
use super::filters::graphical_sub_preprocess_filters;
use super::tonemap::overwrite_color_properties_param;

/// The three filter lists a chain builder produces.
///
/// Port of the `(List<string> MainFilters, List<string> SubFilters,
/// List<string> OverlayFilters)` tuple every `Get*VidFilterChain` returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterChain {
    /// Filters applied to the video stream.
    pub main: Vec<String>,
    /// Filters that prepare the subtitle stream for overlay.
    pub sub: Vec<String>,
    /// Filters that composite the subtitle onto the video.
    pub overlay: Vec<String>,
}

impl FilterChain {
    /// Drops the empty strings the C# builders freely append.
    ///
    /// Port of the three `RemoveAll(string.IsNullOrEmpty)` calls at the top of
    /// `GetVideoProcessingFilterParam`. The builders add unconditionally and
    /// let a helper return `""` to mean "nothing to do", so this is what turns
    /// those into an absent filter rather than an empty one — which ffmpeg
    /// would reject as a syntax error.
    fn prune(&mut self) {
        self.main.retain(|f| !f.is_empty());
        self.sub.retain(|f| !f.is_empty());
        self.overlay.retain(|f| !f.is_empty());
    }
}

/// Where the subtitle and video streams sit, for the `-filter_complex` pads.
///
/// Port of the `mapPrefix` / `subtitleStreamIndex` / `videoStreamIndex` trio.
/// `subtitle_is_external` decides the *input* index: an external subtitle is a
/// second `-i`, so it is input 1 rather than 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPads {
    /// Whether the subtitle comes from a separate file.
    pub subtitle_is_external: bool,
    /// The subtitle's index within its input.
    pub subtitle_index: i32,
    /// The video's index within input 0.
    pub video_index: i32,
}

/// What the subtitle burn-in needs, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleOverlay<'a> {
    /// No subtitle is being burned in.
    None,
    /// A text subtitle, already resolved to its `subtitles=…` filter by the
    /// caller — building it needs the extracted file path, the character set
    /// and the font directory, all of which are I/O.
    Text(&'a str),
    /// A bitmap subtitle, with its own dimensions when known.
    Graphical {
        /// The subtitle bitmap's width.
        width: Option<i32>,
        /// The subtitle bitmap's height.
        height: Option<i32>,
    },
}

/// Everything the software chain reads.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent decision the C# reads separately from \
              its job state; grouping them would invent a taxonomy upstream \
              does not have"
)]
pub struct SwChainInput<'a> {
    /// The persisted encoding options.
    pub options: &'a EncodingOptions,
    /// The selected video encoder name.
    pub video_encoder: &'a str,
    /// Source dimensions.
    pub video_width: Option<i32>,
    /// Source dimensions.
    pub video_height: Option<i32>,
    /// The client-requested output size.
    pub requested: RequestedSize,
    /// The source's stereoscopic layout, if any.
    pub three_d_format: Option<Video3DFormat>,
    /// The source's rotation in degrees.
    pub rotation: Option<i32>,
    /// The source's colour transfer characteristic.
    pub color_transfer: Option<&'a str>,
    /// Whether the job decodes in software (no hardware decoder was selected).
    pub is_sw_decoder: bool,
    /// Whether an H.264/HEVC source is being deinterlaced.
    pub deinterlace: bool,
    /// The source's reference frame rate, for the deinterlacer's rate choice.
    pub reference_frame_rate: Option<f32>,
    /// Whether the software tonemap applies (see
    /// [`super::tonemap::is_sw_tonemap_available`]).
    pub do_tonemap: bool,
    /// Whether the source is Dolby Vision, which the tonemap reshapes.
    pub is_dovi: bool,
    /// The subtitle to burn in, if any.
    pub subtitle: SubtitleOverlay<'a>,
}

/// The software filter chain. Port of `GetSwVidFilterChain`.
#[must_use]
pub fn sw_vid_filter_chain(input: &SwChainInput<'_>) -> FilterChain {
    let options = input.options;
    let encoder = input.video_encoder;
    let is_vaapi_encoder = contains(encoder, "vaapi");
    let is_v4l2_encoder = contains(encoder, "h264_v4l2m2m");

    // A 90-degree rotation means the frame the scaler sees is the transpose of
    // the frame the source declares.
    let swap = input.rotation.unwrap_or(0).abs() == 90;
    let (in_w, in_h) = if swap {
        (input.video_height, input.video_width)
    } else {
        (input.video_width, input.video_height)
    };

    let mut chain = FilterChain::default();

    // State what colour space the frames are in before anything reads them.
    chain.main.push(overwrite_color_properties_param(
        input.color_transfer,
        input.do_tonemap,
    ));

    if input.deinterlace {
        chain.main.push(super::filters::sw_deinterlace_filter(
            options,
            input.reference_frame_rate,
        ));
    }

    // A VAAPI encoder needs nv12 and v4l2 needs yuv420p regardless of anything
    // else; otherwise it follows the decoder, since a hardware decoder hands
    // frames back as nv12.
    let out_format = if is_vaapi_encoder {
        "nv12"
    } else if is_v4l2_encoder || input.is_sw_decoder {
        "yuv420p"
    } else {
        "nv12"
    };

    chain.main.push(sw_scale_filter(
        encoder,
        in_w,
        in_h,
        input.three_d_format,
        input.requested,
    ));

    if input.do_tonemap {
        chain
            .main
            .push(sw_tonemap_filter(options, out_format, input.is_dovi));
    } else {
        chain.main.push(format!("format={out_format}"));
    }

    match input.subtitle {
        SubtitleOverlay::Text(filter) => {
            // A text subtitle is drawn straight onto the video, so it joins the
            // main chain and needs no overlay pass.
            chain.main.push(filter.to_owned());
        }
        SubtitleOverlay::Graphical { width, height } => {
            chain.sub.push(graphical_sub_preprocess_filters(
                in_w,
                in_h,
                width,
                height,
                input.requested,
            ));
            chain
                .overlay
                .push("overlay=eof_action=pass:repeatlast=0".to_owned());
        }
        SubtitleOverlay::None => {}
    }

    chain
}

/// The software tonemap filter. Port of the `tonemapx` block inside
/// `GetSwVidFilterChain`.
///
/// Dolby Vision is the reason this takes the source range: `tonemapx` needs
/// `yuv420p10` input to reshape a DV stream, so the format is forced to
/// `yuv420p` and ffmpeg is left to convert, rather than being handed the nv12
/// the rest of the chain would use.
fn sw_tonemap_filter(options: &EncodingOptions, out_format: &str, is_dovi: bool) -> String {
    let tonemap_format = if is_dovi { "yuv420p" } else { out_format };
    let algorithm = format!("{:?}", options.tonemapping_algorithm);
    let mut args = format!(
        "tonemapx=tonemap={algorithm}:desat={}:peak={}:t=bt709:m=bt709:p=bt709:\
         format={tonemap_format}",
        options.tonemapping_desat, options.tonemapping_peak
    );
    if options.tonemapping_param != 0.0 {
        let _ = write!(args, ":param={}", options.tonemapping_param);
    }
    if matches!(
        options.tonemapping_range,
        TonemappingRange::tv | TonemappingRange::pc
    ) {
        let _ = write!(args, ":range={:?}", options.tonemapping_range);
    }
    args
}

/// The software scale filter. Port of `GetSwScaleFilter`.
///
/// Six shapes, chosen by which of the four size parameters the client supplied.
/// Two details drive the arithmetic:
///
/// - **v4l2 needs 64-pixel width alignment** where everything else needs 2.
///   The hardware simply will not accept anything else.
/// - **MJPEG has to compute the aspect ratio by hand** (`a*sar` rather than
///   `a`), because the encoder does not carry a sample aspect ratio through.
#[must_use]
pub fn sw_scale_filter(
    video_encoder: &str,
    _video_width: Option<i32>,
    _video_height: Option<i32>,
    three_d_format: Option<Video3DFormat>,
    requested: RequestedSize,
) -> String {
    let is_v4l2 = video_encoder.eq_ignore_ascii_case("h264_v4l2m2m");
    let is_mjpeg = contains(video_encoder, "mjpeg");
    let scale_val = if is_v4l2 { 64 } else { 2 };
    // MJPEG carries no sample aspect ratio, so the target has to be computed.
    let ar = if is_mjpeg { "(a*sar)" } else { "a" };

    // Written as C#'s sequence of early returns rather than a `match`, because
    // the ORDER is load-bearing and the two do not agree: these conditions
    // overlap, and a pattern that looks equivalent silently changes which one
    // wins. A request carrying one fixed dimension AND both bounds must take
    // the both-bounds shape, not the one-dimension shape.

    // Both dimensions fixed.
    if let (Some(w), Some(h)) = (requested.width, requested.height) {
        return if is_v4l2 {
            format!("scale=trunc({w}/64)*64:trunc({h}/2)*2")
        } else {
            fixed_sw_scale_filter(three_d_format, w, h)
        };
    }

    // Both bounds given: pick the largest even size inside both.
    if let (Some(max_w), Some(max_h)) = (requested.max_width, requested.max_height) {
        return format!(
            "scale=trunc(min(max(iw\\,ih*{ar})\\,min({max_w}\\,{max_h}*{ar}))/{scale_val})\
             *{scale_val}:trunc(min(max(iw/{ar}\\,ih)\\,min({max_w}/{ar}\\,{max_h}))/2)*2"
        );
    }

    // A fixed width.
    if let Some(w) = requested.width {
        return if three_d_format.is_some() {
            // This shape handles a zero height.
            fixed_sw_scale_filter(three_d_format, w, 0)
        } else {
            format!("scale={w}:trunc(ow/{ar}/2)*2")
        };
    }

    // A fixed height.
    if let Some(h) = requested.height {
        return format!("scale=trunc(oh*{ar}/{scale_val})*{scale_val}:{h}");
    }

    // A width bound.
    if let Some(max_w) = requested.max_width {
        return format!(
            "scale=trunc(min(max(iw\\,ih*{ar})\\,{max_w})/{scale_val})*{scale_val}:\
             trunc(ow/{ar}/2)*2"
        );
    }

    // A height bound.
    if let Some(max_h) = requested.max_height {
        return format!(
            "scale=trunc(oh*{ar}/{scale_val})*{scale_val}:min(max(iw/{ar}\\,ih)\\,{max_h})"
        );
    }

    // Nothing requested: no scaling.
    String::new()
}

/// The scale filter for an exactly-specified size, including the stereoscopic
/// layouts. Port of `GetFixedSwScaleFilter`.
///
/// A side-by-side or top-and-bottom 3D source carries two views in one frame;
/// playing it flat means cropping to one view, restoring the aspect the crop
/// destroyed, and trimming the black bars that leaves.
#[must_use]
pub fn fixed_sw_scale_filter(
    three_d_format: Option<Video3DFormat>,
    requested_width: i32,
    requested_height: i32,
) -> String {
    // The shared tail: fix the display aspect, crop away the bars the earlier
    // steps introduced, reset the sample aspect, then scale to the request.
    const TAIL: &str = "setdar=dar=a,crop=min(iw\\,ih*dar):min(ih\\,iw/dar):\
                        (iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1";

    let w = requested_width;
    if let Some(format) = three_d_format {
        let filter = match format {
            // Half side-by-side: crop to one eye, then stretch it back to full
            // width before the shared tail.
            Video3DFormat::HalfSideBySide => Some(format!(
                "crop=iw/2:ih:0:0,scale=(iw*2):ih,{TAIL},scale={w}:trunc({w}/dar/2)*2"
            )),
            Video3DFormat::FullSideBySide => Some(format!(
                "crop=iw/2:ih:0:0,{TAIL},scale={w}:trunc({w}/dar/2)*2"
            )),
            // Half top-and-bottom. **Accepted divergence:** upstream's string
            // is `scale=(iw*2):ih)` — with an unbalanced closing bracket, a
            // typo present since the case was written. ffmpeg does not tolerate
            // it: the expression parser fails with `[Eval] Invalid char` and the
            // transcode never starts, so half-top-and-bottom 3D playback is
            // broken upstream. Verified both ways against ffmpeg n9.0.1.
            // Ferrofin emits the balanced form, matching what the sibling
            // half-side-by-side case already does correctly.
            //
            // Only the bracket is corrected. The shape itself is upstream's
            // copy-paste — it doubles the *width* after halving the *height*,
            // where a true top-and-bottom stretch would be `scale=iw:(ih*2)` —
            // and that is left alone: it parses and runs, and second-guessing
            // the geometry is beyond what this port is entitled to change.
            Video3DFormat::HalfTopAndBottom => Some(format!(
                "crop=iw:ih/2:0:0,scale=(iw*2):ih,{TAIL},scale={w}:trunc({w}/dar/2)*2"
            )),
            Video3DFormat::FullTopAndBottom => Some(format!(
                "crop=iw:ih/2:0:0,{TAIL},scale={w}:trunc({w}/dar/2)*2"
            )),
            // MVC is not a frame-packed layout, so there is nothing to crop.
            Video3DFormat::Mvc => None,
        };
        if let Some(filter) = filter {
            return filter;
        }
    }

    if requested_height > 0 {
        format!("scale=trunc({w}/2)*2:trunc({requested_height}/2)*2")
    } else {
        format!("scale={w}:trunc({w}/a/2)*2")
    }
}

/// Turns a filter chain into the ffmpeg argument that carries it. Port of the
/// assembly half of `GetVideoProcessingFilterParam`.
///
/// The shape depends entirely on whether anything has to be composited. With no
/// overlay the whole chain is one `-vf`; with an overlay it becomes a
/// `-filter_complex` with named pads, because the subtitle and the video are
/// two separate streams that have to meet. A **text** subtitle skips the input
/// pad entirely — `alphasrc` generates its own source rather than reading one.
#[must_use]
pub fn video_processing_filter_param(
    mut chain: FilterChain,
    framerate: Option<f64>,
    pads: StreamPads,
    has_subtitle: bool,
    subtitle_is_text: bool,
) -> String {
    chain.prune();

    if let Some(framerate) = framerate {
        chain.main.insert(0, format!("fps={framerate}"));
    }

    let main = chain.main.join(",");

    if chain.overlay.is_empty() {
        return if main.is_empty() {
            String::new()
        } else {
            format!(" -vf \"{main}\"")
        };
    }

    if chain.sub.is_empty() || !has_subtitle {
        return String::new();
    }

    let sub = chain.sub.join(",");
    let overlay = chain.overlay.join(",");
    let map_prefix = i32::from(pads.subtitle_is_external);
    let video_index = pads.video_index;

    let graph = if subtitle_is_text {
        // No `[in:idx]` pad: the text chain starts from `alphasrc`.
        if main.is_empty() {
            format!("{sub}[sub];[0:{video_index}][sub]{overlay}")
        } else {
            format!("{sub}[sub];[0:{video_index}]{main}[main];[main][sub]{overlay}")
        }
    } else if main.is_empty() {
        format!(
            "[{map_prefix}:{}]{sub}[sub];[0:{video_index}][sub]{overlay}",
            pads.subtitle_index
        )
    } else {
        format!(
            "[{map_prefix}:{}]{sub}[sub];[0:{video_index}]{main}[main];[main][sub]{overlay}",
            pads.subtitle_index
        )
    };
    format!(" -filter_complex \"{graph}\"")
}

/// `string.Contains(x, StringComparison.OrdinalIgnoreCase)`.
fn contains(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::entities::TonemappingAlgorithm;
    use rstest::rstest;

    // Hand-derived from the C# (10.11.z 3375-3533, 3762-3874, 6160-6265).
    // Upstream ships no tests for any of it.

    fn pads() -> StreamPads {
        StreamPads {
            subtitle_is_external: false,
            subtitle_index: 2,
            video_index: 0,
        }
    }

    fn base_input<'a>(options: &'a EncodingOptions, encoder: &'a str) -> SwChainInput<'a> {
        SwChainInput {
            options,
            video_encoder: encoder,
            video_width: Some(1920),
            video_height: Some(1080),
            requested: RequestedSize::default(),
            three_d_format: None,
            rotation: None,
            color_transfer: None,
            is_sw_decoder: true,
            deinterlace: false,
            reference_frame_rate: Some(25.0),
            do_tonemap: false,
            is_dovi: false,
            subtitle: SubtitleOverlay::None,
        }
    }

    // ----- the software scaler ----------------------------------------------

    #[test]
    fn no_requested_size_means_no_scaling() {
        assert_eq!(
            sw_scale_filter(
                "libx264",
                Some(1920),
                Some(1080),
                None,
                RequestedSize::default()
            ),
            ""
        );
    }

    #[test]
    fn v4l2_aligns_width_to_sixty_four_where_everything_else_uses_two() {
        // The hardware will not accept anything else.
        let requested = RequestedSize {
            width: Some(1280),
            height: Some(720),
            ..RequestedSize::default()
        };
        assert_eq!(
            sw_scale_filter("h264_v4l2m2m", None, None, None, requested),
            "scale=trunc(1280/64)*64:trunc(720/2)*2"
        );
        assert_eq!(
            sw_scale_filter("libx264", None, None, None, requested),
            "scale=trunc(1280/2)*2:trunc(720/2)*2"
        );
    }

    #[test]
    fn mjpeg_computes_the_aspect_ratio_by_hand() {
        // MJPEG carries no sample aspect ratio, so `a` alone is not enough.
        let requested = RequestedSize {
            width: Some(1280),
            ..RequestedSize::default()
        };
        assert_eq!(
            sw_scale_filter("mjpeg", None, None, None, requested),
            "scale=1280:trunc(ow/(a*sar)/2)*2"
        );
        assert_eq!(
            sw_scale_filter("libx264", None, None, None, requested),
            "scale=1280:trunc(ow/a/2)*2"
        );
    }

    #[rstest]
    // Each of the six shapes, keyed by which size parameters were supplied.
    #[case(
        RequestedSize { width: Some(1280), height: Some(720), ..RequestedSize::default() },
        "scale=trunc(1280/2)*2:trunc(720/2)*2"
    )]
    #[case(
        RequestedSize { width: Some(1280), ..RequestedSize::default() },
        "scale=1280:trunc(ow/a/2)*2"
    )]
    #[case(
        RequestedSize { height: Some(720), ..RequestedSize::default() },
        "scale=trunc(oh*a/2)*2:720"
    )]
    #[case(
        RequestedSize { max_width: Some(1280), ..RequestedSize::default() },
        "scale=trunc(min(max(iw\\,ih*a)\\,1280)/2)*2:trunc(ow/a/2)*2"
    )]
    #[case(
        RequestedSize { max_height: Some(720), ..RequestedSize::default() },
        "scale=trunc(oh*a/2)*2:min(max(iw/a\\,ih)\\,720)"
    )]
    #[case(
        RequestedSize { max_width: Some(1280), max_height: Some(720), ..RequestedSize::default() },
        "scale=trunc(min(max(iw\\,ih*a)\\,min(1280\\,720*a))/2)*2:\
         trunc(min(max(iw/a\\,ih)\\,min(1280/a\\,720))/2)*2"
    )]
    fn each_combination_of_size_parameters_has_its_own_shape(
        #[case] requested: RequestedSize,
        #[case] expected: &str,
    ) {
        assert_eq!(
            sw_scale_filter("libx264", Some(1920), Some(1080), None, requested),
            expected
        );
    }

    #[rstest]
    // The conditions OVERLAP, and upstream resolves that by testing them in a
    // fixed order: both-bounds beats one-fixed-dimension. These two inputs are
    // the only ones where the ordering is observable, and getting it wrong
    // silently drops the bounds and changes the output resolution.
    #[case(RequestedSize {
        width: Some(1280),
        max_width: Some(1920),
        max_height: Some(1080),
        ..RequestedSize::default()
    })]
    #[case(RequestedSize {
        height: Some(720),
        max_width: Some(1920),
        max_height: Some(1080),
        ..RequestedSize::default()
    })]
    fn both_bounds_win_over_a_single_fixed_dimension(#[case] requested: RequestedSize) {
        assert_eq!(
            sw_scale_filter("libx264", Some(1920), Some(1080), None, requested),
            "scale=trunc(min(max(iw\\,ih*a)\\,min(1920\\,1080*a))/2)*2:\
             trunc(min(max(iw/a\\,ih)\\,min(1920/a\\,1080))/2)*2"
        );
    }

    #[test]
    fn v4l2_alignment_reaches_the_bounded_shapes_too() {
        // `scaleVal` is interpolated twice in each bounded shape; the
        // fixed-both branch spells 64 on its own line and so does not cover it.
        let bounded = RequestedSize {
            max_width: Some(1920),
            max_height: Some(1080),
            ..RequestedSize::default()
        };
        assert_eq!(
            sw_scale_filter("h264_v4l2m2m", Some(1920), Some(1080), None, bounded),
            "scale=trunc(min(max(iw\\,ih*a)\\,min(1920\\,1080*a))/64)*64:\
             trunc(min(max(iw/a\\,ih)\\,min(1920/a\\,1080))/2)*2"
        );
        let width_bound = RequestedSize {
            max_width: Some(1920),
            ..RequestedSize::default()
        };
        assert_eq!(
            sw_scale_filter("h264_v4l2m2m", Some(1920), Some(1080), None, width_bound),
            "scale=trunc(min(max(iw\\,ih*a)\\,1920)/64)*64:trunc(ow/a/2)*2"
        );
        let height = RequestedSize {
            height: Some(720),
            ..RequestedSize::default()
        };
        assert_eq!(
            sw_scale_filter("h264_v4l2m2m", Some(1920), Some(1080), None, height),
            "scale=trunc(oh*a/64)*64:720"
        );
    }

    // ----- stereoscopic sources ---------------------------------------------

    #[rstest]
    // Side-by-side crops horizontally; top-and-bottom crops vertically. The
    // "half" variants additionally stretch the surviving view back to size.
    #[case(Video3DFormat::HalfSideBySide, "crop=iw/2:ih:0:0,scale=(iw*2):ih,")]
    #[case(Video3DFormat::FullSideBySide, "crop=iw/2:ih:0:0,")]
    #[case(Video3DFormat::HalfTopAndBottom, "crop=iw:ih/2:0:0,scale=(iw*2):ih,")]
    #[case(Video3DFormat::FullTopAndBottom, "crop=iw:ih/2:0:0,")]
    fn a_frame_packed_3d_source_is_cropped_to_one_view(
        #[case] format: Video3DFormat,
        #[case] expected_prefix: &str,
    ) {
        let filter = fixed_sw_scale_filter(Some(format), 1280, 720);
        assert!(filter.starts_with(expected_prefix), "{filter}");
        assert!(
            filter.ends_with("scale=1280:trunc(1280/dar/2)*2"),
            "{filter}"
        );
    }

    #[test]
    fn the_half_top_and_bottom_filter_balances_upstreams_stray_bracket() {
        // Upstream writes `scale=(iw*2):ih)`, with one bracket too many. That
        // is not cosmetic: ffmpeg's expression parser rejects it outright
        // (`[Eval] Invalid chars ')' at the end of expression 'ih)'`) and the transcode never starts, so this 3D
        // layout does not play on Jellyfin at all. Ferrofin emits the balanced
        // form — the same shape the half-side-by-side case already has.
        let filter = fixed_sw_scale_filter(Some(Video3DFormat::HalfTopAndBottom), 1280, 720);
        assert!(filter.contains("scale=(iw*2):ih,"), "{filter}");
        assert!(!filter.contains("ih)"), "{filter}");
        // Brackets balance across the whole filter, which is what the parser
        // actually requires.
        assert_eq!(
            filter.matches('(').count(),
            filter.matches(')').count(),
            "{filter}"
        );
    }

    #[rstest]
    // The shared tail — restore the display aspect the crop destroyed, trim the
    // bars that leaves, reset the sample aspect — is what makes the crop
    // watchable, and it is long enough that one mangled character would go
    // unnoticed. Pinned in full, including upstream's odd spacing inside
    // `(ih - min (ih\,ih/sar))`.
    #[case(
        Video3DFormat::HalfSideBySide,
        "crop=iw/2:ih:0:0,scale=(iw*2):ih,setdar=dar=a,crop=min(iw\\,ih*dar):\
min(ih\\,iw/dar):(iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1,\
scale=1280:trunc(1280/dar/2)*2"
    )]
    #[case(
        Video3DFormat::FullSideBySide,
        "crop=iw/2:ih:0:0,setdar=dar=a,crop=min(iw\\,ih*dar):\
min(ih\\,iw/dar):(iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1,\
scale=1280:trunc(1280/dar/2)*2"
    )]
    #[case(
        Video3DFormat::HalfTopAndBottom,
        "crop=iw:ih/2:0:0,scale=(iw*2):ih,setdar=dar=a,crop=min(iw\\,ih*dar):\
min(ih\\,iw/dar):(iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1,\
scale=1280:trunc(1280/dar/2)*2"
    )]
    #[case(
        Video3DFormat::FullTopAndBottom,
        "crop=iw:ih/2:0:0,setdar=dar=a,crop=min(iw\\,ih*dar):\
min(ih\\,iw/dar):(iw-min(iw\\,iw*sar))/2:(ih - min (ih\\,ih/sar))/2,setsar=sar=1,\
scale=1280:trunc(1280/dar/2)*2"
    )]
    fn each_3d_layout_has_a_full_string_golden(
        #[case] format: Video3DFormat,
        #[case] expected: &str,
    ) {
        assert_eq!(fixed_sw_scale_filter(Some(format), 1280, 720), expected);
    }

    #[test]
    fn mvc_is_not_frame_packed_so_it_scales_normally() {
        // Multiview Video Coding carries its second view in a separate layer,
        // not side by side, so there is nothing to crop.
        assert_eq!(
            fixed_sw_scale_filter(Some(Video3DFormat::Mvc), 1280, 720),
            "scale=trunc(1280/2)*2:trunc(720/2)*2"
        );
    }

    #[test]
    fn a_zero_height_request_derives_the_height_from_the_aspect() {
        assert_eq!(
            fixed_sw_scale_filter(None, 1280, 0),
            "scale=1280:trunc(1280/a/2)*2"
        );
    }

    // ----- the software chain ------------------------------------------------

    #[test]
    fn the_plain_chain_states_the_colour_space_and_the_output_format() {
        let options = EncodingOptions::default();
        let chain = sw_vid_filter_chain(&base_input(&options, "libx264"));
        assert_eq!(
            chain.main,
            vec![
                "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709".to_owned(),
                String::new(),
                "format=yuv420p".to_owned(),
            ]
        );
        assert!(chain.sub.is_empty());
        assert!(chain.overlay.is_empty());
    }

    #[rstest]
    // The output format follows whoever consumes the frames next.
    #[case("h264_vaapi", true, "format=nv12")]
    #[case("h264_v4l2m2m", true, "format=yuv420p")]
    #[case("libx264", true, "format=yuv420p")]
    // A hardware decoder hands back nv12, so a software encoder keeps it.
    #[case("libx264", false, "format=nv12")]
    fn the_output_format_follows_the_next_consumer(
        #[case] encoder: &str,
        #[case] is_sw_decoder: bool,
        #[case] expected: &str,
    ) {
        let options = EncodingOptions::default();
        let mut input = base_input(&options, encoder);
        input.is_sw_decoder = is_sw_decoder;
        let chain = sw_vid_filter_chain(&input);
        assert!(
            chain.main.contains(&expected.to_owned()),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn a_ninety_degree_rotation_transposes_the_scalers_input() {
        // The scaler sees the frame the source declares, which for a rotated
        // source is the transpose of what will be displayed.
        let options = EncodingOptions::default();
        let mut input = base_input(&options, "libx264");
        input.rotation = Some(90);
        input.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        input.requested = RequestedSize {
            max_width: Some(1920),
            max_height: Some(1920),
            ..RequestedSize::default()
        };
        let chain = sw_vid_filter_chain(&input);
        // 1920x1080 rotated is 1080x1920, so the subtitle canvas is portrait
        // and its aspect no longer matches the 16:9 subtitle — which sends it
        // down the pad-and-crop chain. Asserting the whole string matters: an
        // unrotated run produces the equal-aspect rescale instead, and both
        // contain "1080".
        assert_eq!(
            chain.sub[0],
            "scale,scale=-1:1920:fast_bilinear,crop,pad=max(1080\\,iw):max(1920\\,ih):\
             (ow-iw)/2:(oh-ih)/2:black@0,crop=1080:1920"
        );

        // Without the rotation the same job takes the equal-aspect path.
        let mut upright = input;
        upright.rotation = None;
        assert_eq!(
            sw_vid_filter_chain(&upright).sub[0],
            "scale,scale=1920:1080:fast_bilinear"
        );
    }

    #[test]
    fn a_text_subtitle_joins_the_main_chain_with_no_overlay_pass() {
        let options = EncodingOptions::default();
        let mut input = base_input(&options, "libx264");
        input.subtitle = SubtitleOverlay::Text("subtitles=f='/media/a.ass'");
        let chain = sw_vid_filter_chain(&input);
        assert_eq!(chain.main.last().unwrap(), "subtitles=f='/media/a.ass'");
        assert!(chain.sub.is_empty());
        assert!(chain.overlay.is_empty());
    }

    #[test]
    fn a_graphical_subtitle_gets_its_own_chain_and_an_overlay() {
        let options = EncodingOptions::default();
        let mut input = base_input(&options, "libx264");
        input.subtitle = SubtitleOverlay::Graphical {
            width: Some(1920),
            height: Some(1080),
        };
        let chain = sw_vid_filter_chain(&input);
        assert_eq!(chain.sub.len(), 1);
        assert_eq!(chain.overlay, vec!["overlay=eof_action=pass:repeatlast=0"]);
        // The main chain does NOT carry the subtitle.
        assert!(!chain.main.iter().any(|f| f.contains("overlay")));
    }

    #[test]
    fn deinterlacing_is_inserted_before_the_scale() {
        let options = EncodingOptions::default();
        let mut input = base_input(&options, "libx264");
        input.deinterlace = true;
        let chain = sw_vid_filter_chain(&input);
        assert_eq!(chain.main[1], "yadif=0:-1:0");
    }

    #[test]
    fn tonemapping_replaces_the_plain_format_filter() {
        let options = EncodingOptions::default();
        let mut input = base_input(&options, "libx264");
        input.do_tonemap = true;
        let chain = sw_vid_filter_chain(&input);
        assert!(!chain.main.iter().any(|f| f == "format=yuv420p"));
        assert_eq!(
            chain.main.last().unwrap(),
            "tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p"
        );
        // ...and the colour properties now describe the HDR input.
        assert!(chain.main[0].contains("bt2020"), "{:?}", chain.main);
    }

    #[test]
    fn dolby_vision_forces_the_tonemap_to_yuv420p() {
        // `tonemapx` needs yuv420p10 input to reshape a DV stream, so the
        // format is forced even when the rest of the chain would use nv12.
        let options = EncodingOptions::default();
        let mut input = base_input(&options, "libx264");
        input.do_tonemap = true;
        input.is_dovi = true;
        input.is_sw_decoder = false; // would otherwise be nv12
        let chain = sw_vid_filter_chain(&input);
        assert!(
            chain.main.last().unwrap().ends_with("format=yuv420p"),
            "{:?}",
            chain.main
        );
    }

    #[test]
    fn the_software_tonemap_carries_its_optional_arguments() {
        let options = EncodingOptions {
            tonemapping_algorithm: TonemappingAlgorithm::reinhard,
            tonemapping_param: 0.5,
            tonemapping_range: TonemappingRange::tv,
            ..EncodingOptions::default()
        };
        let mut input = base_input(&options, "libx264");
        input.do_tonemap = true;
        let chain = sw_vid_filter_chain(&input);
        let filter = chain.main.last().unwrap();
        assert!(filter.contains("tonemap=reinhard"), "{filter}");
        assert!(filter.contains(":param=0.5"), "{filter}");
        assert!(filter.contains(":range=tv"), "{filter}");
    }

    // ----- assembling the argument -------------------------------------------

    #[test]
    fn a_chain_with_no_overlay_becomes_a_single_vf() {
        let chain = FilterChain {
            main: vec!["format=yuv420p".to_owned(), "scale=1280:720".to_owned()],
            ..FilterChain::default()
        };
        assert_eq!(
            video_processing_filter_param(chain, None, pads(), false, false),
            " -vf \"format=yuv420p,scale=1280:720\""
        );
    }

    #[test]
    fn empty_filters_are_pruned_rather_than_emitted() {
        // The builders append unconditionally and let helpers return "" to mean
        // "nothing to do"; an empty filter would be an ffmpeg syntax error.
        let chain = FilterChain {
            main: vec![String::new(), "format=yuv420p".to_owned(), String::new()],
            ..FilterChain::default()
        };
        assert_eq!(
            video_processing_filter_param(chain, None, pads(), false, false),
            " -vf \"format=yuv420p\""
        );
        // A chain that prunes to nothing produces no argument at all.
        let empty = FilterChain {
            main: vec![String::new()],
            ..FilterChain::default()
        };
        assert_eq!(
            video_processing_filter_param(empty, None, pads(), false, false),
            ""
        );
    }

    #[test]
    fn all_three_filter_lists_are_pruned_not_just_the_main_one() {
        // Not cosmetic: `graphical_sub_preprocess_filters` returns "" whenever
        // the output size is unknown, so a real job reaches here with an empty
        // sub filter. Without the prune the assembly emits a filtergraph
        // referencing a `[sub]` pad that nothing defines.
        let empty_sub = FilterChain {
            main: vec!["format=yuv420p".to_owned()],
            sub: vec![String::new()],
            overlay: vec!["overlay=eof_action=pass:repeatlast=0".to_owned()],
        };
        assert_eq!(
            video_processing_filter_param(empty_sub, None, pads(), true, false),
            ""
        );

        // An overlay list that prunes to nothing falls back to the `-vf` path.
        let empty_overlay = FilterChain {
            main: vec!["format=yuv420p".to_owned()],
            sub: vec!["scale=1280:720".to_owned()],
            overlay: vec![String::new()],
        };
        assert_eq!(
            video_processing_filter_param(empty_overlay, None, pads(), true, false),
            " -vf \"format=yuv420p\""
        );

        // ...and an empty entry in each list is dropped without disturbing the
        // rest of the graph.
        let all_three = FilterChain {
            main: vec![String::new(), "format=yuv420p".to_owned()],
            sub: vec![String::new(), "scale=1280:720".to_owned()],
            overlay: vec![String::new(), "overlay".to_owned()],
        };
        assert_eq!(
            video_processing_filter_param(all_three, None, pads(), true, false),
            " -filter_complex \"[0:2]scale=1280:720[sub];[0:0]format=yuv420p[main];\
[main][sub]overlay\""
        );
    }

    #[test]
    fn a_framerate_is_inserted_at_the_head_of_the_main_chain() {
        let chain = FilterChain {
            main: vec!["format=yuv420p".to_owned()],
            ..FilterChain::default()
        };
        assert_eq!(
            video_processing_filter_param(chain, Some(23.976), pads(), false, false),
            " -vf \"fps=23.976,format=yuv420p\""
        );
    }

    #[test]
    fn a_graphical_overlay_becomes_a_filter_complex_with_an_input_pad() {
        let chain = FilterChain {
            main: vec!["format=yuv420p".to_owned()],
            sub: vec!["scale=1280:720".to_owned()],
            overlay: vec!["overlay=eof_action=pass:repeatlast=0".to_owned()],
        };
        assert_eq!(
            video_processing_filter_param(chain, None, pads(), true, false),
            " -filter_complex \"[0:2]scale=1280:720[sub];[0:0]format=yuv420p[main];\
             [main][sub]overlay=eof_action=pass:repeatlast=0\""
        );
    }

    #[test]
    fn an_external_subtitle_is_a_second_input() {
        let chain = FilterChain {
            main: vec!["format=yuv420p".to_owned()],
            sub: vec!["scale=1280:720".to_owned()],
            overlay: vec!["overlay".to_owned()],
        };
        let external = StreamPads {
            subtitle_is_external: true,
            subtitle_index: 0,
            video_index: 0,
        };
        let args = video_processing_filter_param(chain, None, external, true, false);
        assert!(args.contains("[1:0]scale=1280:720[sub]"), "{args}");
    }

    #[test]
    fn a_text_overlay_has_no_input_pad_because_alphasrc_is_its_own_source() {
        let chain = FilterChain {
            main: vec!["format=nv12".to_owned()],
            sub: vec!["alphasrc=s=1280x720:r=25:start='0'".to_owned()],
            overlay: vec!["overlay_cuda".to_owned()],
        };
        let args = video_processing_filter_param(chain, None, pads(), true, true);
        assert_eq!(
            args,
            " -filter_complex \"alphasrc=s=1280x720:r=25:start='0'[sub];\
             [0:0]format=nv12[main];[main][sub]overlay_cuda\""
        );
        assert!(!args.contains("[0:2]"), "{args}");
    }

    #[test]
    fn an_empty_main_chain_leaves_the_video_pad_unfiltered() {
        let chain = FilterChain {
            main: Vec::new(),
            sub: vec!["scale=1280:720".to_owned()],
            overlay: vec!["overlay".to_owned()],
        };
        assert_eq!(
            video_processing_filter_param(chain, None, pads(), true, false),
            " -filter_complex \"[0:2]scale=1280:720[sub];[0:0][sub]overlay\""
        );
    }

    #[test]
    fn an_overlay_with_nothing_to_overlay_produces_no_argument() {
        // Upstream falls through to `string.Empty` rather than emitting a
        // filtergraph that references a `[sub]` pad nothing defines.
        let chain = FilterChain {
            main: vec!["format=yuv420p".to_owned()],
            sub: Vec::new(),
            overlay: vec!["overlay".to_owned()],
        };
        assert_eq!(
            video_processing_filter_param(chain.clone(), None, pads(), true, false),
            ""
        );
        // ...and the same when the job has no subtitle stream at all.
        let with_sub = FilterChain {
            sub: vec!["scale=1280:720".to_owned()],
            ..chain
        };
        assert_eq!(
            video_processing_filter_param(with_sub, None, pads(), false, false),
            ""
        );
    }
}
