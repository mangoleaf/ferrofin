//! `DrawingUtils` — port of `MediaBrowser.Model.Drawing.DrawingUtils`.
//!
//! Aspect-preserving resize math. The C# original rounds via
//! `Convert.ToInt32(double)`, which uses banker's rounding (round half to even);
//! [`round_to_i32`] replicates that exactly so the ported behavior matches.

use super::ImageDimensions;

/// Resizes a set of dimensions.
///
/// Mirrors `DrawingUtils.Resize`. A positive `width`/`height` fixes that axis; a
/// positive `max_width`/`max_height` caps that axis (preserving aspect ratio).
/// Non-positive values mean "unset".
#[must_use]
pub fn resize(
    size: ImageDimensions,
    width: i32,
    height: i32,
    max_width: i32,
    max_height: i32,
) -> ImageDimensions {
    let mut new_width = size.width;
    let mut new_height = size.height;

    if width > 0 && height > 0 {
        new_width = width;
        new_height = height;
    } else if height > 0 {
        new_width = get_new_width(new_height, new_width, height);
        new_height = height;
    } else if width > 0 {
        new_height = get_new_height(new_height, new_width, width);
        new_width = width;
    }

    if max_height > 0 && max_height < new_height {
        new_width = get_new_width(new_height, new_width, max_height);
        new_height = max_height;
    }

    if max_width > 0 && max_width < new_width {
        new_height = get_new_height(new_height, new_width, max_width);
        new_width = max_width;
    }

    ImageDimensions::new(new_width, new_height)
}

/// Scales down to fill a box, returning the original size if both `fill_width`
/// and `fill_height` are `None`/zero.
///
/// Mirrors `DrawingUtils.ResizeFill`.
#[must_use]
pub fn resize_fill(
    size: ImageDimensions,
    fill_width: Option<i32>,
    fill_height: Option<i32>,
) -> ImageDimensions {
    // Return original size if input is invalid.
    if matches!(fill_width, None | Some(0)) && matches!(fill_height, None | Some(0)) {
        return size;
    }

    let fill_width = match fill_width {
        None | Some(0) => 1,
        Some(w) => w,
    };
    let fill_height = match fill_height {
        None | Some(0) => 1,
        Some(h) => h,
    };

    let width_ratio = f64::from(size.width) / f64::from(fill_width);
    let height_ratio = f64::from(size.height) / f64::from(fill_height);
    let scale_ratio = width_ratio.min(height_ratio);

    // Clamp to current size.
    if scale_ratio < 1.0 {
        return size;
    }

    // Ceil of a bounded dimension ratio fits i32 for realistic image sizes.
    #[allow(clippy::cast_possible_truncation)]
    let new_width = (f64::from(size.width) / scale_ratio).ceil() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let new_height = (f64::from(size.height) / scale_ratio).ceil() as i32;

    ImageDimensions::new(new_width, new_height)
}

/// Gets the new width when scaling to a target height.
fn get_new_width(current_height: i32, current_width: i32, new_height: i32) -> i32 {
    round_to_i32(f64::from(new_height) / f64::from(current_height) * f64::from(current_width))
}

/// Gets the new height when scaling to a target width.
fn get_new_height(current_height: i32, current_width: i32, new_width: i32) -> i32 {
    round_to_i32(f64::from(new_width) / f64::from(current_width) * f64::from(current_height))
}

/// Rounds a `f64` to an `i32` using round-half-to-even (banker's rounding),
/// matching C# `Convert.ToInt32(double)`.
fn round_to_i32(value: f64) -> i32 {
    let floor = value.floor();
    let diff = value - floor;
    let rounded = if diff > 0.5 {
        floor + 1.0
    } else if diff < 0.5 {
        floor
    } else {
        // Exactly halfway: round to even.
        #[allow(clippy::cast_possible_truncation)]
        let floor_is_even = (floor as i64) % 2 == 0;
        if floor_is_even { floor } else { floor + 1.0 }
    };
    // The rounded aspect-ratio result fits i32 for realistic image dimensions.
    #[allow(clippy::cast_possible_truncation)]
    let out = rounded as i32;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_fixed_width_and_height() {
        let out = resize(ImageDimensions::new(1920, 1080), 640, 480, 0, 0);
        assert_eq!(ImageDimensions::new(640, 480), out);
    }

    #[test]
    fn resize_fixed_width_preserves_aspect() {
        let out = resize(ImageDimensions::new(1920, 1080), 960, 0, 0, 0);
        assert_eq!(ImageDimensions::new(960, 540), out);
    }

    #[test]
    fn resize_fixed_height_preserves_aspect() {
        let out = resize(ImageDimensions::new(1920, 1080), 0, 540, 0, 0);
        assert_eq!(ImageDimensions::new(960, 540), out);
    }

    #[test]
    fn resize_max_width_caps() {
        let out = resize(ImageDimensions::new(1920, 1080), 0, 0, 960, 0);
        assert_eq!(ImageDimensions::new(960, 540), out);
    }

    #[test]
    fn resize_fill_returns_original_when_unset() {
        let size = ImageDimensions::new(100, 100);
        assert_eq!(size, resize_fill(size, None, None));
        assert_eq!(size, resize_fill(size, Some(0), Some(0)));
    }

    #[test]
    fn resize_fill_scales_down() {
        let out = resize_fill(ImageDimensions::new(1920, 1080), Some(960), Some(540));
        assert_eq!(ImageDimensions::new(960, 540), out);
    }

    #[test]
    fn resize_fill_clamps_when_smaller_than_fill() {
        let size = ImageDimensions::new(100, 100);
        assert_eq!(size, resize_fill(size, Some(200), Some(200)));
    }

    #[test]
    fn round_to_i32_banker_rounding() {
        assert_eq!(2, round_to_i32(2.5));
        assert_eq!(2, round_to_i32(1.5));
        assert_eq!(4, round_to_i32(3.5));
        assert_eq!(3, round_to_i32(2.6));
        assert_eq!(2, round_to_i32(2.4));
    }
}
