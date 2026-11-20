//! Real-ffmpeg integration test for [`TrickplayFrameExtractorImpl`] — the
//! trickplay frame-extraction run over the concrete [`TokioTranscoder`].
//!
//! Exercises the un-mockable piece (the real process spawn plus the ported
//! `fps`/`scale` filter chain) against a live ffmpeg on a tiny generated clip.
//! `#[ignore]`d, so a default run lists it as skipped; asked for, it needs
//! `ffmpeg` on `PATH`.
//!
//! Run with:
//! `cargo test -p ferrofin-mediaencoding --test trickplay_frames_ffmpeg -- --include-ignored`

mod colour_bars;

use std::path::Path;
use std::sync::Arc;

use colour_bars::{
    BARS, BARS_FPS, BARS_HEIGHT, BARS_WIDTH, COLOUR_TOLERANCE, Matrix, bar_colours, make_bars_clip,
};

use ferrofin_mediaencoding::encoder::trickplay_extractor::MIN_FFMPEG_JPEG_MATRIX_CONVERSION;
use ferrofin_mediaencoding::{
    EncoderValidator, FfmpegVersion, TokioTranscoder, TrickplayFrameExtractorImpl,
};
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::media_encoding::{
    TrickplayExtraction, TrickplayFrameExtractor as _, TrickplayOutput,
};

/// The version of the `ffmpeg` on `PATH`, probed the way the composition root
/// probes it.
///
/// Passing `None` here instead would be a lie the test tells itself: the
/// unprobed branch emits the deprecated `-vsync`, which ffmpeg **removed** in
/// 8.0, so on a modern build the run dies with "Unrecognized option 'vsync'"
/// — a failure production never sees, because production always probes.
fn probed_ffmpeg_version() -> Option<FfmpegVersion> {
    let out = std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .ok()?;
    EncoderValidator::new("ffmpeg")
        .get_ffmpeg_version_internal(&String::from_utf8_lossy(&out.stdout))
}

/// The stream ffprobe reports for [`make_clip`]'s output: 128x72 H.264 at
/// 10 fps.
///
/// Production always hands the extractor the item's probed video stream (the
/// trickplay manager refuses a source without one), and a full decode rebuilds
/// timestamps at that stream's frame rate (`setpts=N/10.000/TB`). Passing no
/// stream would rebuild them at the 30 fps fallback and squeeze this 10 fps
/// clip into a third of its length — a failure production never sees.
fn clip_stream() -> MediaStream {
    MediaStream {
        codec: Some("h264".to_owned()),
        index: 0,
        stream_type: MediaStreamType::Video,
        width: Some(128),
        height: Some(72),
        average_frame_rate: Some(10.0),
        real_frame_rate: Some(10.0),
        ..MediaStream::default()
    }
}

/// A 2 s-interval, 64 px-wide software extraction request.
///
/// Hardware is off here deliberately: this test runs on whatever ffmpeg and
/// whatever GPU (or none) the machine has, so the accelerated path is not
/// reproducible enough to assert on. The hardware argument builders are
/// covered by unit goldens; this proves the process spawn and the software
/// filter chain against a live ffmpeg.
fn extraction<'a>(
    input_path: &'a str,
    video_stream: &'a MediaStream,
    output_dir: &'a str,
) -> TrickplayExtraction<'a> {
    TrickplayExtraction {
        input_path,
        video_stream: Some(video_stream),
        media_streams: std::slice::from_ref(video_stream),
        interval_ms: 2_000,
        max_width: 64,
        qscale: 4,
        threads: 1,
        priority: ferrofin_model::configuration::ProcessPriorityClass::BelowNormal,
        output_dir,
        allow_hw_accel: false,
        enable_hw_encoding: false,
        keyframe_only: false,
        full_chroma: false,
    }
}

/// Whether a program is on `PATH` (via `<prog> -version`).
fn on_path(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Panics unless `ffmpeg` is on `PATH`: these tests are `#[ignore]`d, so a
/// run that asks for them must not pass without it.
fn require_ffmpeg() {
    assert!(on_path("ffmpeg"), "ffmpeg not found on PATH");
}

/// Generates a tiny 6-second silent `testsrc` clip at `path` (mirrors the
/// fixture generator in `segment_transcode_ffmpeg.rs`, minus the audio the
/// extractor drops anyway).
fn make_clip(path: &Path) {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=6:size=128x72:rate=10",
            "-c:v",
            "libx264",
            // 4:2:0, as real media is. `testsrc` is RGB, which libx264 would
            // otherwise keep at 4:4:4.
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn ffmpeg for clip");
    assert!(status.success(), "clip generation failed");
    assert!(
        std::fs::metadata(path).is_ok_and(|m| m.len() > 0),
        "generated clip is empty"
    );
}

#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn extracts_interval_frames_from_a_real_clip() {
    require_ffmpeg();

    let tmp = tempfile::tempdir().expect("tempdir");
    let clip = tmp.path().join("clip (with spaces).mp4");
    make_clip(&clip);

    let out_dir = tmp.path().join("frames");
    let extractor = TrickplayFrameExtractorImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        probed_ffmpeg_version(),
    );

    // A 6 s clip sampled every 2 s at 64 px max width.
    let frames = extractor
        .extract_trickplay_frames(&extraction(
            &clip.to_string_lossy(),
            &clip_stream(),
            &out_dir.to_string_lossy(),
        ))
        .await
        .expect("frame extraction succeeds");

    // fps=1/2 over 6 s yields 3 frames (the muxer may round one extra in).
    assert!(
        (3..=4).contains(&frames.len()),
        "expected 3-4 frames, got {}: {frames:?}",
        frames.len()
    );
    assert!(frames.windows(2).all(|w| w[0] < w[1]), "sorted order");
    for frame in &frames {
        let path = Path::new(frame);
        assert!(path.is_file(), "frame exists: {frame}");
        let bytes = std::fs::read(path).expect("read frame");
        assert!(!bytes.is_empty(), "frame is non-empty: {frame}");
        // JPEG magic — the mjpeg encoder really wrote images.
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG SOI marker in {frame}");
    }
}

/// The (horizontal, vertical) sampling factors of every component of a JPEG,
/// in the order Y, Cb, Cr.
fn sampling_factors(path: &str) -> Vec<(u8, u8)> {
    let bytes = std::fs::read(path).expect("read frame");
    let mut i = 2; // skip SOI
    loop {
        assert_eq!(bytes[i], 0xFF, "not a JPEG marker at offset {i}");
        if bytes[i + 1] == 0xC0 {
            // SOF0: FF C0, length, precision, height, width, count, then
            // (id, sampling, table) per component.
            return (0..usize::from(bytes[i + 9]))
                .map(|component| {
                    let sampling = bytes[i + 11 + 3 * component];
                    (sampling >> 4, sampling & 0x0F)
                })
                .collect();
        }
        i += 2 + ((usize::from(bytes[i + 2]) << 8) | usize::from(bytes[i + 3]));
    }
}

/// ffmpeg really writes the chroma asked for, on a 4:2:0 source. The argument
/// tests cannot show that the mjpeg encoder takes the format filter's pixels.
/// 4:4:4 is chroma sampled as often as luma, whatever factors a build declares
/// for that: ffmpeg writes `(1, 2)` on every component where others write
/// `(1, 1)`.
#[rstest::rstest]
#[case::full_chroma(true)]
#[case::jellyfin_chroma(false)]
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn frames_are_written_at_the_chroma_asked_for(#[case] full_chroma: bool) {
    require_ffmpeg();

    let tmp = tempfile::tempdir().expect("tempdir");
    let clip = tmp.path().join("clip.mp4");
    make_clip(&clip);

    let out_dir = tmp.path().join("frames");
    let extractor = TrickplayFrameExtractorImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        probed_ffmpeg_version(),
    );
    let frames = extractor
        .extract_trickplay_frames(&TrickplayExtraction {
            full_chroma,
            ..extraction(
                &clip.to_string_lossy(),
                &clip_stream(),
                &out_dir.to_string_lossy(),
            )
        })
        .await
        .expect("frame extraction succeeds");

    assert!((3..=4).contains(&frames.len()), "{frames:?}");
    for frame in &frames {
        let factors = sampling_factors(frame);
        assert_eq!(factors.len(), 3, "{frame}");
        let chroma_matches_luma = factors[1] == factors[0] && factors[2] == factors[0];
        assert_eq!(chroma_matches_luma, full_chroma, "{frame}: {factors:?}");
    }
}

/// The stream ffprobe reports for [`make_bars_clip`]'s output.
fn bars_stream() -> MediaStream {
    MediaStream {
        width: Some(i32::from(BARS_WIDTH)),
        height: Some(i32::from(BARS_HEIGHT)),
        average_frame_rate: Some(f32::from(BARS_FPS)),
        real_frame_rate: Some(f32::from(BARS_FPS)),
        ..clip_stream()
    }
}

/// A JPEG decoder reads Y'CbCr as BT.601, full range (JFIF), and so does the
/// tile packer that decodes these frames. Frames of a BT.709 source must be
/// converted to that matrix, or HD and 4K previews shift colour (gl-1129).
/// BT.601 and untagged sources are already in it and must not be converted
/// again: ffmpeg reads an untagged source as BT.601. Below ffmpeg 5.1 nothing
/// is converted, so the BT.709 cases are skipped there.
#[rstest::rstest]
#[case::bt709(
    Matrix::Bt709,
    Some("colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv"),
    false
)]
#[case::bt709_full_chroma(
    Matrix::Bt709,
    Some("colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv"),
    true
)]
#[case::bt601(
    Matrix::Bt601,
    Some("colorspace=smpte170m:color_primaries=smpte170m:color_trc=smpte170m:range=tv"),
    false
)]
#[case::untagged(Matrix::Bt601, None, false)]
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn frames_keep_the_colours_of_the_source(
    #[case] matrix: Matrix,
    #[case] tags: Option<&str>,
    #[case] full_chroma: bool,
) {
    require_ffmpeg();

    let version = probed_ffmpeg_version();
    if matches!(matrix, Matrix::Bt709)
        && version.is_none_or(|v| v < MIN_FFMPEG_JPEG_MATRIX_CONVERSION)
    {
        let found = version.map_or_else(|| "unprobed".to_owned(), |v| v.to_string());
        eprintln!(
            "skipping: ffmpeg ({found}) is unprobed or older than \
             {MIN_FFMPEG_JPEG_MATRIX_CONVERSION}, where trickplay converts no matrix"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let clip = tmp.path().join("bars.mkv");
    make_bars_clip(&clip, matrix, tags);

    let out_dir = tmp.path().join("frames");
    let extractor =
        TrickplayFrameExtractorImpl::new(Arc::new(TokioTranscoder::new()), "ffmpeg", version);
    let stream = bars_stream();
    let frames = extractor
        .extract_trickplay_frames(&TrickplayExtraction {
            max_width: 128,
            full_chroma,
            ..extraction(&clip.to_string_lossy(), &stream, &out_dir.to_string_lossy())
        })
        .await
        .expect("frame extraction succeeds");

    assert!(!frames.is_empty(), "no frames");
    for frame in &frames {
        let image = image::open(frame).expect("decode frame").to_rgb8();
        let (width, height) = image.dimensions();
        assert_eq!((width, height), (128, 72), "{frame}");
        let bar_width = width / u32::try_from(BARS.len()).expect("a few bars");
        let decoded: Vec<[u8; 3]> = (0..)
            .take(BARS.len())
            .map(|bar| {
                image
                    .get_pixel(bar_width * bar + bar_width / 2, height / 2)
                    .0
            })
            .collect();
        let off = decoded
            .iter()
            .zip(&BARS)
            .flat_map(|(got, want)| got.iter().zip(want).map(|(g, w)| g.abs_diff(*w)))
            .max()
            .unwrap_or_default();
        assert!(
            off <= COLOUR_TOLERANCE,
            "{frame}: decoded {decoded:?}, source {BARS:?} (off by {off})"
        );
    }
}

/// The tail the hardware builder appends to frames a GPU decoder hands the
/// software MJPEG encoder: a conversion at the frame's own size, from the
/// matrix the frames hold to the JPEG's BT.601 (gl-1157). Downloaded frames are
/// `yuv420p` from CUDA and `nv12` from VAAPI and QSV, full range when the VPP
/// scaler was told the encoder is MJPEG, and a same-size scale must convert
/// each, which needs no GPU to show.
#[rstest::rstest]
#[case::cuda_download("scale=128:72,format=yuv420p")]
#[case::vaapi_qsv_download("scale=128:72,format=nv12")]
#[case::vaapi_qsv_full_range_download("scale=128:72:out_range=pc,format=nv12")]
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn a_downloaded_frame_is_converted_to_bt601_at_its_own_size(#[case] download: &str) {
    require_ffmpeg();
    let version = probed_ffmpeg_version();
    if version.is_none_or(|v| v < MIN_FFMPEG_JPEG_MATRIX_CONVERSION) {
        eprintln!("skipping: ffmpeg is unprobed or older than {MIN_FFMPEG_JPEG_MATRIX_CONVERSION}");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let clip = tmp.path().join("bars.mkv");
    make_bars_clip(
        &clip,
        Matrix::Bt709,
        Some("colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv"),
    );
    let frame = tmp.path().join("frame.jpg");
    // A hardware scaler's fixed-size output in the download's format, then
    // the builder's tail as `build_accelerated_trickplay_args` writes it: keep
    // the two in step.
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&clip)
        .args([
            "-vf",
            &format!("{download},scale=in_color_matrix=bt709:out_color_matrix=bt601"),
            "-frames:v",
            "1",
            "-c:v",
            "mjpeg",
            "-qscale:v",
            "4",
        ])
        .arg(&frame)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed");

    let got = bar_colours(&image::open(&frame).expect("decode frame").to_rgb8());
    let off = got
        .iter()
        .zip(&BARS)
        .flat_map(|(g, w)| g.iter().zip(w).map(|(g, w)| g.abs_diff(*w)))
        .max()
        .unwrap_or_default();
    assert!(
        off <= COLOUR_TOLERANCE,
        "{download}: decoded {got:?}, source {BARS:?} (off by {off})"
    );
}

/// Whether the `ffmpeg` on `PATH` has jellyfin-ffmpeg's `tonemapx`, the
/// filter software trickplay tone-maps an HDR source with.
fn has_tonemapx() -> bool {
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some("tonemapx"))
        })
}

/// [`BARS`] as an HDR10 video holds them, as 10-bit limited-range BT.2020
/// Y'CbCr: each colour linearised (BT.1886, gamma 2.4) with white at
/// 100 cd/m², moved to BT.2020 primaries and PQ-encoded (SMPTE ST 2084).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "each value is rounded and lies in 64..=960"
)]
fn hdr10_ycbcr(rgb: [u8; 3]) -> [u16; 3] {
    const BT709_TO_BT2020: [[f64; 3]; 3] = [
        [0.6274, 0.3293, 0.0433],
        [0.0691, 0.9195, 0.0114],
        [0.0164, 0.0880, 0.8956],
    ];
    let pq = |nits: f64| {
        let (m1, m2) = (2610.0 / 16384.0, 2523.0 / 4096.0 * 128.0);
        let (c1, c2, c3) = (
            3424.0 / 4096.0,
            2413.0 / 4096.0 * 32.0,
            2392.0 / 4096.0 * 32.0,
        );
        let y = (nits / 10_000.0).powf(m1);
        ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
    };
    let linear = rgb.map(|c| (f64::from(c) / 255.0).powf(2.4) * 100.0);
    let [r, g, b] = BT709_TO_BT2020.map(|row| pq(row.iter().zip(linear).map(|(k, c)| k * c).sum()));
    let (kr, kb) = (0.2627, 0.0593);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));
    [64.0 + 876.0 * y, 512.0 + 896.0 * cb, 512.0 + 896.0 * cr].map(|v| v.round() as u16)
}

/// Writes a 4 s HDR10 clip of [`BARS`] side by side at `path`, losslessly
/// coded (FFV1, 10-bit 4:2:0) and tagged BT.2020/PQ.
fn make_hdr10_bars_clip(path: &Path) {
    const WIDTH: usize = BARS_WIDTH as usize;
    const HEIGHT: usize = BARS_HEIGHT as usize;
    const FRAMES: usize = 4 * BARS_FPS as usize;
    let bar_width = WIDTH / BARS.len();
    let values = BARS.map(hdr10_ycbcr);
    let mut frame = Vec::with_capacity(WIDTH * HEIGHT * 3);
    for _ in 0..HEIGHT {
        frame.extend((0..WIDTH).flat_map(|x| values[x / bar_width][0].to_le_bytes()));
    }
    for plane in [1, 2] {
        for _ in 0..HEIGHT / 2 {
            frame.extend(
                (0..WIDTH / 2).flat_map(|x| values[2 * x / bar_width][plane].to_le_bytes()),
            );
        }
    }
    let raw = path.with_extension("yuv");
    std::fs::write(&raw, frame.repeat(FRAMES)).expect("write raw bars");

    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-f", "rawvideo", "-pix_fmt", "yuv420p10le"])
        .args([
            "-video_size",
            &format!("{WIDTH}x{HEIGHT}"),
            "-framerate",
            &BARS_FPS.to_string(),
        ])
        .arg("-i")
        .arg(&raw)
        .args([
            "-vf",
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=tv",
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p10le",
        ])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn ffmpeg for the HDR bars clip");
    assert!(status.success(), "HDR bars clip generation failed");
}

/// A configuration whose encoding options are the defaults. The extractor
/// reads nothing else, and reads even those only when trickplay may use
/// hardware.
struct DefaultEncoding;

#[async_trait::async_trait]
impl ferrofin_traits::configuration::ServerConfigurationManager for DefaultEncoding {
    async fn get_encoding_options(
        &self,
    ) -> Result<ferrofin_model::configuration::EncodingOptions, ferrofin_traits::error::ServiceError>
    {
        Ok(ferrofin_model::configuration::EncodingOptions::default())
    }
    fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
        unimplemented!("trickplay reads only the encoding options")
    }
    async fn configuration(
        &self,
    ) -> Result<
        Arc<ferrofin_model::configuration::ServerConfiguration>,
        ferrofin_traits::error::ServiceError,
    > {
        unimplemented!("trickplay reads only the encoding options")
    }
    async fn update_configuration(
        &self,
        _configuration: &ferrofin_model::configuration::ServerConfiguration,
    ) -> Result<(), ferrofin_traits::error::ServiceError> {
        unimplemented!("trickplay reads only the encoding options")
    }
    async fn get_branding(
        &self,
    ) -> Result<ferrofin_model::branding::BrandingOptions, ferrofin_traits::error::ServiceError>
    {
        unimplemented!("trickplay reads only the encoding options")
    }
    async fn update_branding(
        &self,
        _branding: &ferrofin_model::branding::BrandingOptions,
    ) -> Result<(), ferrofin_traits::error::ServiceError> {
        unimplemented!("trickplay reads only the encoding options")
    }
}

/// Software trickplay tone-maps an HDR10 source as upstream does, and the
/// frame then keeps `tonemapx`'s colours: they are BT.709, and a JPEG decoder
/// reads BT.601, so the graph converts after the tone-map (gl-1159). The
/// reference is `tonemapx`'s output read as BT.709 by ffmpeg itself. An
/// un-tone-mapped frame misses it by about 55 and one read with the wrong
/// matrix by about 16. Needs jellyfin-ffmpeg (for `tonemapx`), 5.1 or later.
#[rstest::rstest]
#[case::jellyfin_chroma(false)]
#[case::full_chroma(true)]
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn an_hdr10_source_is_tone_mapped_to_its_sdr_colours(#[case] full_chroma: bool) {
    require_ffmpeg();
    if !has_tonemapx() {
        eprintln!("skipping: ffmpeg has no tonemapx (jellyfin-ffmpeg has it)");
        return;
    }
    let version = probed_ffmpeg_version();
    let Some(version) = version.filter(|v| *v >= MIN_FFMPEG_JPEG_MATRIX_CONVERSION) else {
        let found = version.map_or_else(|| "unprobed".to_owned(), |v| v.to_string());
        eprintln!(
            "skipping: ffmpeg ({found}) is unprobed or older than \
             {MIN_FFMPEG_JPEG_MATRIX_CONVERSION}, where trickplay converts no matrix"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let clip = tmp.path().join("hdr10.mkv");
    make_hdr10_bars_clip(&clip);

    // The tone-map of `EncodingOptions::default()`, which a job with trickplay
    // hardware off reads (upstream's `new EncodingOptions()`).
    let reference = tmp.path().join("reference.png");
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&clip)
        .args([
            "-vf",
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,\
             scale=128:72,\
             tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p,\
             scale=in_color_matrix=bt709:in_range=tv,format=rgb24",
            "-frames:v",
            "1",
        ])
        .arg(&reference)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn ffmpeg for the reference");
    assert!(status.success(), "reference tone-map failed");
    let want = bar_colours(&image::open(&reference).expect("decode reference").to_rgb8());

    let caps = ferrofin_mediaencoding::FfmpegCapabilities::builder()
        .filters(["tonemapx"])
        .ffmpeg_version(version)
        .build();
    let extractor =
        TrickplayFrameExtractorImpl::new(Arc::new(TokioTranscoder::new()), "ffmpeg", Some(version))
            .with_hardware(Arc::new(caps), Arc::new(DefaultEncoding));
    let stream = MediaStream {
        codec: Some("ffv1".to_owned()),
        pixel_format: Some("yuv420p10le".to_owned()),
        color_transfer: Some("smpte2084".to_owned()),
        color_primaries: Some("bt2020".to_owned()),
        color_space: Some("bt2020nc".to_owned()),
        ..bars_stream()
    };
    let out_dir = tmp.path().join("frames");
    let frames = extractor
        .extract_trickplay_frames(&TrickplayExtraction {
            max_width: 128,
            full_chroma,
            ..extraction(&clip.to_string_lossy(), &stream, &out_dir.to_string_lossy())
        })
        .await
        .expect("frame extraction succeeds");

    assert!(!frames.is_empty(), "no frames");
    for frame in &frames {
        let image = image::open(frame).expect("decode frame").to_rgb8();
        assert_eq!(image.dimensions(), (128, 72), "{frame}");
        let got = bar_colours(&image);
        let off = got
            .iter()
            .zip(&want)
            .flat_map(|(g, w)| g.iter().zip(w).map(|(g, w)| g.abs_diff(*w)))
            .max()
            .unwrap_or_default();
        assert!(
            off <= COLOUR_TOLERANCE,
            "{frame}: decoded {got:?}, tone-mapped reference {want:?} (off by {off})"
        );
    }
}

/// The real transcoder, recording each run's arguments and whether it
/// succeeded, so a test can tell a shared run from its per-width fallback.
#[derive(Default)]
struct RecordedRuns {
    inner: TokioTranscoder,
    runs: std::sync::Mutex<Vec<(String, bool)>>,
}

#[async_trait::async_trait]
impl ferrofin_mediaencoding::Transcoder for RecordedRuns {
    async fn get_process_output(
        &self,
        path: &str,
        arguments: &str,
        read_stderr: bool,
        test_key: Option<&str>,
        env: &[(String, String)],
    ) -> Result<ferrofin_mediaencoding::encoder::ProcessOutput, String> {
        let output = self
            .inner
            .get_process_output(path, arguments, read_stderr, test_key, env)
            .await;
        let success = output.as_ref().is_ok_and(|o| o.success);
        self.runs
            .lock()
            .expect("runs lock")
            .push((arguments.to_owned(), success));
        output
    }

    async fn get_process_exit_code(&self, path: &str, arguments: &str) -> bool {
        self.inner.get_process_exit_code(path, arguments).await
    }
}

/// The bytes of every frame in `frames`, in order.
fn frame_bytes(frames: &[String]) -> Vec<Vec<u8>> {
    frames
        .iter()
        .map(|frame| std::fs::read(frame).expect("read frame"))
        .collect()
}

/// Several widths made by one run come out byte for byte as each width's own
/// run makes them (gl-1228): the shared filters and each branch's are the
/// width's own graph, in its order. On every software graph shape: a
/// progressive source, an interlaced one (deinterlaced once, sampled on each
/// branch), full chroma, keyframe-only, and an HDR10 source tone-mapped
/// behind each scale or, with full chroma, once ahead of them (with
/// jellyfin-ffmpeg's `tonemapx` only).
#[rstest::rstest]
#[case::progressive(false, false, false, false)]
#[case::interlaced(true, false, false, false)]
#[case::full_chroma(false, true, false, false)]
#[case::keyframe_only(false, false, true, false)]
#[case::hdr(false, false, false, true)]
#[case::hdr_full_chroma(false, true, false, true)]
#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn several_widths_from_one_decode_match_their_own_runs(
    #[case] interlaced: bool,
    #[case] full_chroma: bool,
    #[case] keyframe_only: bool,
    #[case] hdr: bool,
) {
    require_ffmpeg();
    if hdr && !has_tonemapx() {
        eprintln!("skipping: ffmpeg has no tonemapx (jellyfin-ffmpeg has it)");
        return;
    }
    let Some(version) = probed_ffmpeg_version() else {
        eprintln!("skipping: the ffmpeg version could not be probed");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let (clip, stream) = if hdr {
        let clip = tmp.path().join("hdr10.mkv");
        make_hdr10_bars_clip(&clip);
        let stream = MediaStream {
            codec: Some("ffv1".to_owned()),
            pixel_format: Some("yuv420p10le".to_owned()),
            color_transfer: Some("smpte2084".to_owned()),
            color_primaries: Some("bt2020".to_owned()),
            color_space: Some("bt2020nc".to_owned()),
            ..bars_stream()
        };
        (clip, stream)
    } else {
        let clip = tmp.path().join("clip.mp4");
        make_clip(&clip);
        (clip, clip_stream())
    };
    let stream = MediaStream {
        is_interlaced: interlaced,
        ..stream
    };
    let caps = ferrofin_mediaencoding::FfmpegCapabilities::builder()
        .filters(hdr.then_some("tonemapx"))
        .ffmpeg_version(version)
        .build();
    let transcoder = Arc::new(RecordedRuns::default());
    let extractor =
        TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg", Some(version))
            .with_hardware(Arc::new(caps), Arc::new(DefaultEncoding));
    let clip = clip.to_string_lossy();
    let request = TrickplayExtraction {
        full_chroma,
        keyframe_only,
        ..extraction(&clip, &stream, "/unused")
    };
    let widths = [32, 48, 64];
    let dir = |kind: &str, width: i32| {
        tmp.path()
            .join(format!("{kind}{width}"))
            .to_string_lossy()
            .into_owned()
    };
    let shared_dirs: Vec<String> = widths.iter().map(|w| dir("shared", *w)).collect();
    let outputs: Vec<TrickplayOutput<'_>> = widths
        .iter()
        .zip(&shared_dirs)
        .map(|(max_width, output_dir)| TrickplayOutput {
            max_width: *max_width,
            output_dir,
        })
        .collect();

    let shared = extractor
        .extract_trickplay_frame_sets(&request, &outputs)
        .await;
    // One run made them all, not a fallback to a run per width. A
    // keyframe-only run may be retried the standard way first, as one run
    // too: this clip's single keyframe makes the system ffmpeg's MJPEG
    // encoder refuse the keyframe-only frame, as it does a width's own run.
    {
        let runs = transcoder.runs.lock().expect("runs lock");
        assert!(
            runs.iter().all(|(args, _)| args.contains("split=3")),
            "{runs:?}"
        );
        assert!(runs.last().is_some_and(|(_, success)| *success), "{runs:?}");
        assert!(runs.len() <= if keyframe_only { 2 } else { 1 }, "{runs:?}");
    }

    for (output, set) in outputs.iter().zip(shared) {
        let set = set.expect("the shared run makes every width");
        let own_dir = dir("own", output.max_width);
        let own = extractor
            .extract_trickplay_frames(&TrickplayExtraction {
                max_width: output.max_width,
                output_dir: &own_dir,
                ..request
            })
            .await
            .expect("the width's own run");
        assert_eq!(set.len(), own.len(), "width {}", output.max_width);
        assert!(
            frame_bytes(&set) == frame_bytes(&own),
            "width {}: the shared run's frames differ from the width's own",
            output.max_width
        );
        let (width, _) = image::image_dimensions(&set[0]).expect("frame size");
        assert_eq!(i32::try_from(width).ok(), Some(output.max_width));
    }
}

#[tokio::test]
#[ignore = "runs the real ffmpeg; run with --ignored"]
async fn missing_input_is_an_error() {
    require_ffmpeg();

    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("frames");
    let extractor = TrickplayFrameExtractorImpl::new(
        Arc::new(TokioTranscoder::new()),
        "ffmpeg",
        probed_ffmpeg_version(),
    );

    let err = extractor
        .extract_trickplay_frames(&extraction(
            &tmp.path().join("nope.mp4").to_string_lossy(),
            &clip_stream(),
            &out_dir.to_string_lossy(),
        ))
        .await
        .expect_err("missing input must fail");
    assert!(
        err.to_string().contains("no trickplay frames"),
        "diagnostic mentions the empty run: {err}"
    );
}
