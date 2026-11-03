//! Flat colour bars for the ffmpeg-gated tests that check the colours of the
//! JPEGs ffmpeg writes (trickplay frames, chapter images): a clip whose
//! Y'CbCr is computed here under a chosen matrix, and the colours read back
//! from a decoded image.

use std::path::Path;

/// The colour bars of [`make_bars_clip`], as sRGB: a red, a green, a blue and
/// a skin tone.
pub const BARS: [[u8; 3]; 4] = [[192, 32, 64], [32, 160, 64], [40, 60, 200], [220, 170, 130]];

/// The size and frame rate of [`make_bars_clip`]'s clip.
pub const BARS_WIDTH: u16 = 256;
pub const BARS_HEIGHT: u16 = 144;
pub const BARS_FPS: u8 = 5;

/// How far a decoded bar may stray from [`BARS`], per channel. A test value:
/// a trickplay frame JPEG of a flat bar lands within 2 of it, a chapter image
/// converted from BT.709 within 3 (4 when the dashboard's resolution scales
/// it), and BT.709 values read as BT.601 miss by up to 21.
pub const COLOUR_TOLERANCE: u8 = 4;

/// A Y'CbCr matrix, by its luma coefficients.
#[derive(Debug, Clone, Copy)]
pub enum Matrix {
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
    pub fn ycbcr(self, rgb: [u8; 3]) -> [u8; 3] {
        let (kr, kb) = self.coefficients();
        let [r, g, b] = rgb.map(|c| f64::from(c) / 255.0);
        let y = kr * r + (1.0 - kr - kb) * g + kb * b;
        let cb = (b - y) / (2.0 * (1.0 - kb));
        let cr = (r - y) / (2.0 * (1.0 - kr));
        [16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr].map(|v| v.round() as u8)
    }
}

/// Writes a 4 s clip of [`BARS`] side by side at `path`, losslessly coded.
///
/// The Y'CbCr is computed here under `matrix` and handed to ffmpeg raw, so no
/// ffmpeg conversion decides what the source holds. `tags` is the `setparams`
/// that labels it, which changes no pixel; `None` leaves it untagged.
pub fn make_bars_clip(path: &Path, matrix: Matrix, tags: Option<&str>) {
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

/// The bars' colours at the middle of `image`, left to right.
pub fn bar_colours(image: &image::RgbImage) -> Vec<[u8; 3]> {
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
