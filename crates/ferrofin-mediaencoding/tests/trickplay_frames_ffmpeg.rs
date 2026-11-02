//! Real-ffmpeg integration test for [`TrickplayFrameExtractorImpl`] — the
//! trickplay frame-extraction run over the concrete [`TokioTranscoder`].
//!
//! Exercises the un-mockable piece (the real process spawn plus the ported
//! `fps`/`scale` filter chain) against a live ffmpeg on a tiny generated clip.
//! Skips itself unless `FERROFIN_FFMPEG_TESTS` is set *and* `ffmpeg` is on
//! `PATH`, so ffmpeg-less CI stays green.
//!
//! Run with:
//! `FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-mediaencoding --test trickplay_frames_ffmpeg`

use std::path::Path;
use std::sync::Arc;

use ferrofin_mediaencoding::encoder::trickplay_extractor::MIN_FFMPEG_JPEG_MATRIX_CONVERSION;
use ferrofin_mediaencoding::{
    EncoderValidator, FfmpegVersion, TokioTranscoder, TrickplayFrameExtractorImpl,
};
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::media_encoding::{TrickplayExtraction, TrickplayFrameExtractor as _};

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

/// Whether the ffmpeg-gated suite should run: `FERROFIN_FFMPEG_TESTS` set AND
/// `ffmpeg` present. Prints a skip line and returns `false` otherwise.
fn ffmpeg_gate() -> bool {
    if std::env::var("FERROFIN_FFMPEG_TESTS").is_err() {
        eprintln!("skipping: FERROFIN_FFMPEG_TESTS not set");
        return false;
    }
    if !on_path("ffmpeg") {
        eprintln!("skipping: ffmpeg not found on PATH");
        return false;
    }
    true
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
async fn extracts_interval_frames_from_a_real_clip() {
    if !ffmpeg_gate() {
        return;
    }

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
async fn frames_are_written_at_the_chroma_asked_for(#[case] full_chroma: bool) {
    if !ffmpeg_gate() {
        return;
    }

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

/// The colour bars of [`make_bars_clip`], as sRGB: a red, a green, a blue and
/// a skin tone.
const BARS: [[u8; 3]; 4] = [[192, 32, 64], [32, 160, 64], [40, 60, 200], [220, 170, 130]];

/// The size and frame rate of [`make_bars_clip`]'s clip.
const BARS_WIDTH: u16 = 256;
const BARS_HEIGHT: u16 = 144;
const BARS_FPS: u8 = 5;

/// How far a decoded bar may stray from [`BARS`], per channel. A test value:
/// a frame JPEG of a flat bar lands within 2 of it, and BT.709 values read
/// as BT.601 miss by up to 21.
const COLOUR_TOLERANCE: u8 = 4;

/// A Y'CbCr matrix, by its luma coefficients.
#[derive(Debug, Clone, Copy)]
enum Matrix {
    Bt709,
    Bt601,
}

impl Matrix {
    /// `(Kr, Kb)`.
    fn coefficients(self) -> (f64, f64) {
        match self {
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt601 => (0.299, 0.114),
        }
    }

    /// `rgb` as 8-bit limited-range Y'CbCr under this matrix.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "each value is rounded and lies in 16..=240"
    )]
    fn ycbcr(self, rgb: [u8; 3]) -> [u8; 3] {
        let (kr, kb) = self.coefficients();
        let [r, g, b] = rgb.map(|c| f64::from(c) / 255.0);
        let y = kr * r + (1.0 - kr - kb) * g + kb * b;
        let cb = (b - y) / (2.0 * (1.0 - kb));
        let cr = (r - y) / (2.0 * (1.0 - kr));
        [16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr].map(|v| v.round() as u8)
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

/// Writes a 4 s clip of [`BARS`] side by side at `path`, losslessly coded.
///
/// The Y'CbCr is computed here under `matrix` and handed to ffmpeg raw, so no
/// ffmpeg conversion decides what the source holds. `tags` is the `setparams`
/// that labels it, which changes no pixel; `None` leaves it untagged.
fn make_bars_clip(path: &Path, matrix: Matrix, tags: Option<&str>) {
    const WIDTH: usize = BARS_WIDTH as usize;
    const HEIGHT: usize = BARS_HEIGHT as usize;
    const FRAMES: usize = 4 * BARS_FPS as usize;
    let bar_width = WIDTH / BARS.len();
    let values = BARS.map(|rgb| matrix.ycbcr(rgb));
    let mut frame = Vec::with_capacity(WIDTH * HEIGHT * 3 / 2);
    for _ in 0..HEIGHT {
        frame.extend((0..WIDTH).map(|x| values[x / bar_width][0]));
    }
    for plane in [1, 2] {
        for _ in 0..HEIGHT / 2 {
            frame.extend((0..WIDTH / 2).map(|x| values[2 * x / bar_width][plane]));
        }
    }
    let raw = path.with_extension("yuv");
    std::fs::write(&raw, frame.repeat(FRAMES)).expect("write raw bars");

    let mut ffmpeg = std::process::Command::new("ffmpeg");
    ffmpeg
        .args(["-y", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .args([
            "-video_size",
            &format!("{WIDTH}x{HEIGHT}"),
            "-framerate",
            &BARS_FPS.to_string(),
        ])
        .arg("-i")
        .arg(&raw);
    if let Some(tags) = tags {
        ffmpeg.args(["-vf", &format!("setparams={tags}")]);
    }
    let status = ffmpeg
        .args(["-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p"])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn ffmpeg for the bars clip");
    assert!(status.success(), "bars clip generation failed");
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
async fn frames_keep_the_colours_of_the_source(
    #[case] matrix: Matrix,
    #[case] tags: Option<&str>,
    #[case] full_chroma: bool,
) {
    if !ffmpeg_gate() {
        return;
    }

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

/// The bars' colours at the middle of `image`, left to right.
fn bar_colours(image: &image::RgbImage) -> Vec<[u8; 3]> {
    let (width, height) = image.dimensions();
    let bar_width = width / u32::try_from(BARS.len()).expect("a few bars");
    (0..)
        .take(BARS.len())
        .map(|bar| {
            image
                .get_pixel(bar_width * bar + bar_width / 2, height / 2)
                .0
        })
        .collect()
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
async fn an_hdr10_source_is_tone_mapped_to_its_sdr_colours(#[case] full_chroma: bool) {
    if !ffmpeg_gate() {
        return;
    }
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

#[tokio::test]
async fn missing_input_is_an_error() {
    if !ffmpeg_gate() {
        return;
    }

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
