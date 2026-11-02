//! The generated splashscreen — port of `Jellyfin.Drawing.Skia.SplashscreenBuilder`.
//!
//! A wall of posters and landscape images, six rows on a black `5760x2160`
//! canvas, drawn through upstream's fixed perspective matrix onto the final
//! `1920x1080` frame. Each row starts its image counter at a random `0..5`;
//! counter values `0`, `2` and `3` take the next poster, `1` and `4` the next
//! backdrop, cycling through each list (`SkiaHelper.GetNextValidImage` wraps).
//!
//! Like upstream's `canvas.DrawBitmap` with default sampling, the wall is
//! sampled nearest-neighbour through the matrix.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};

use ferrofin_traits::error::ServiceError;
use image::{Rgba, RgbaImage, imageops};

use crate::image_encoder::{ImageCrateEncoder, next_valid};

/// The output frame (`FinalWidth`/`FinalHeight`).
const FINAL_WIDTH: u32 = 1920;
const FINAL_HEIGHT: u32 = 1080;
/// The wall, larger than the frame so the tilt never shows its edge.
const WALL_WIDTH: u32 = FINAL_WIDTH * 3;
const WALL_HEIGHT: u32 = FINAL_HEIGHT * 2;
const ROWS: u32 = 6;
/// The gap between two images and between two rows.
const SPACING: u32 = 20;
/// How far each row starts further right than the one above (`i * 75`).
const ROW_INDENT: u32 = 75;

/// Upstream's `Transform3D` matrix, row-major: `[ScaleX, SkewX, TransX]`,
/// `[SkewY, ScaleY, TransY]`, `[Persp0, Persp1, Persp2]`. It maps a wall point
/// to a frame point.
const WALL_TO_FRAME: [[f64; 3]; 3] = [
    [0.324_108_899, -0.244_337_708, 42.040_771_5],
    [0.037_760_902_2, 0.563_934_922, -198.104_706],
    [-9.089_593_37e-05, 6.852_420_48e-05, 0.988_209_724],
];

/// The splashscreen for `posters` and `backdrops`, or
/// [`ServiceError::InvalidInput`] when either list has no decodable image
/// (`GenerateCollage`'s "Not enough valid pictures").
pub(crate) fn generate_splash(
    posters: &[String],
    backdrops: &[String],
) -> Result<RgbaImage, ServiceError> {
    Ok(transform_3d(&generate_collage(posters, backdrops)?))
}

/// Port of `GenerateCollage`.
fn generate_collage(posters: &[String], backdrops: &[String]) -> Result<RgbaImage, ServiceError> {
    let mut wall = RgbaImage::from_pixel(WALL_WIDTH, WALL_HEIGHT, Rgba([0, 0, 0, 0xFF]));
    let row_height = WALL_HEIGHT / ROWS;
    // Each path is decoded and scaled once, where upstream decodes it for every
    // tile it fills; the wall comes out the same.
    let mut scaled: HashMap<String, Option<RgbaImage>> = HashMap::new();
    let mut load = |path: &str| -> Option<RgbaImage> {
        scaled
            .entry(path.to_owned())
            .or_insert_with(|| {
                let image = ImageCrateEncoder::decode(path).ok()?;
                let height = image.height().max(1);
                let width = (u64::from(row_height) * u64::from(image.width()) / u64::from(height))
                    .clamp(1, u64::from(WALL_WIDTH));
                Some(
                    image
                        .resize_exact(
                            u32::try_from(width).unwrap_or(WALL_WIDTH),
                            row_height,
                            imageops::FilterType::Lanczos3,
                        )
                        .to_rgba8(),
                )
            })
            .clone()
    };
    let mut poster_index = 0;
    let mut backdrop_index = 0;
    for row in 0..ROWS {
        let mut counter = random_below(5);
        let mut x = row * ROW_INDENT;
        let y = row * (row_height + SPACING);
        while x < WALL_WIDTH {
            let (paths, index) = if matches!(counter, 0 | 2 | 3) {
                (posters, &mut poster_index)
            } else {
                (backdrops, &mut backdrop_index)
            };
            let Some((image, next)) = next_valid(paths, *index, &mut load) else {
                return Err(ServiceError::invalid_input(
                    "Not enough valid pictures provided to create a splashscreen!",
                ));
            };
            *index = next;
            imageops::overlay(&mut wall, &image, i64::from(x), i64::from(y));
            x += image.width() + SPACING;
            counter = if counter >= 4 { 0 } else { counter + 1 };
        }
    }
    Ok(wall)
}

/// Port of `Transform3D`: each frame pixel's centre is mapped back through the
/// inverse matrix and takes the wall pixel it lands on, black off the wall.
fn transform_3d(wall: &RgbaImage) -> RgbaImage {
    let inverse = invert(WALL_TO_FRAME);
    RgbaImage::from_fn(FINAL_WIDTH, FINAL_HEIGHT, |u, v| {
        let (fu, fv) = (f64::from(u) + 0.5, f64::from(v) + 0.5);
        let project = |row: [f64; 3]| row[0] * fu + row[1] * fv + row[2];
        let w = project(inverse[2]);
        let (x, y) = (project(inverse[0]) / w, project(inverse[1]) / w);
        let on_wall = w > 0.0
            && (0.0..f64::from(wall.width())).contains(&x)
            && (0.0..f64::from(wall.height())).contains(&y);
        if on_wall {
            // In range by the check above, so the casts cannot truncate.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pixel = *wall.get_pixel(x as u32, y as u32);
            pixel
        } else {
            Rgba([0, 0, 0, 0xFF])
        }
    })
}

/// The inverse of a 3x3 matrix (adjugate over determinant). The splashscreen
/// matrix is a fixed, invertible projection.
fn invert(m: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let cofactor =
        |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    let adjugate = [
        [
            cofactor(1, 2, 1, 2),
            -cofactor(0, 2, 1, 2),
            cofactor(0, 1, 1, 2),
        ],
        [
            -cofactor(1, 2, 0, 2),
            cofactor(0, 2, 0, 2),
            -cofactor(0, 1, 0, 2),
        ],
        [
            cofactor(1, 2, 0, 1),
            -cofactor(0, 2, 0, 1),
            cofactor(0, 1, 0, 1),
        ],
    ];
    let determinant =
        m[0][0] * adjugate[0][0] + m[0][1] * adjugate[1][0] + m[0][2] * adjugate[2][0];
    adjugate.map(|row| row.map(|value| value / determinant))
}

/// A random integer in `0..bound` (upstream's `Random.Shared.Next(0, bound)`).
///
/// ponytail: std's per-instance random `RandomState` keys as the source, so no
/// RNG dependency; plenty for picking where a row starts.
fn random_below(bound: u32) -> u32 {
    let value = RandomState::new().hash_one(0u8) % u64::from(bound);
    u32::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inverse undoes the matrix: a wall point comes back where it began.
    #[test]
    fn the_inverse_matrix_round_trips_a_wall_point() {
        let inverse = invert(WALL_TO_FRAME);
        let apply = |m: [[f64; 3]; 3], p: [f64; 3]| {
            m.map(|row| row[0] * p[0] + row[1] * p[1] + row[2] * p[2])
        };
        let frame = apply(WALL_TO_FRAME, [2880.0, 1080.0, 1.0]);
        let wall = apply(inverse, frame);
        assert!((wall[0] / wall[2] - 2880.0).abs() < 1e-6, "{wall:?}");
        assert!((wall[1] / wall[2] - 1080.0).abs() < 1e-6, "{wall:?}");
    }

    #[test]
    fn random_below_stays_in_range() {
        assert!((0..200).all(|_| random_below(5) < 5));
    }
}
