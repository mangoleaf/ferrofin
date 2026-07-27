//! Ports of `MediaBrowser.Controller.Drawing.ImageProcessingOptions` and
//! `ImageCollageOptions`.
//!
//! Port rule applied: the C# `BaseItem Item` field (the whole domain item) is
//! dropped in favour of an [`item_id`](ImageProcessingOptions::item_id)
//! [`Uuid`] plus the [`ItemImageInfo`] row being processed — the encoder only
//! needs the identity and the image, not the OOP item tree.

use hermit_model::drawing::{ImageDimensions, ImageFormat};
use uuid::Uuid;

use super::ItemImageInfo;

/// The JPEG quality at or above which an image is treated as "unmodified" for
/// the purposes of [`ImageProcessingOptions::has_default_options`]. This is the
/// C# `>= 90` threshold; exposed as a named constant rather than a bare literal.
const DEFAULT_QUALITY_THRESHOLD: i32 = 90;

/// Describes a single image-processing request: which image, at what size, with
/// what overlays.
///
/// Mirrors C# `ImageProcessingOptions`. `RequiresAutoOrientation` defaults to
/// `true` (the C# constructor default); every other field is the type's zero
/// value, so [`Default`] reproduces the C# `new ImageProcessingOptions()`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageProcessingOptions {
    /// The id of the item the image belongs to (replaces C# `BaseItem Item`).
    pub item_id: Uuid,

    /// The image being processed.
    pub image: ItemImageInfo,

    /// The index of the image within its type.
    pub image_index: i32,

    /// The exact target width, if fixed.
    pub width: Option<i32>,

    /// The exact target height, if fixed.
    pub height: Option<i32>,

    /// The maximum width the output may have.
    pub max_width: Option<i32>,

    /// The maximum height the output may have.
    pub max_height: Option<i32>,

    /// The width to fill (letterbox/crop target).
    pub fill_width: Option<i32>,

    /// The height to fill (letterbox/crop target).
    pub fill_height: Option<i32>,

    /// The output encoding quality (0–100).
    pub quality: i32,

    /// The output formats the caller can accept, in preference order.
    pub supported_output_formats: Vec<ImageFormat>,

    /// An unplayed-count badge to burn into the image, if any.
    pub unplayed_count: Option<i32>,

    /// A blur radius to apply, if any.
    pub blur: Option<i32>,

    /// The played-percentage overlay (0.0–100.0).
    pub percent_played: f64,

    /// A background colour to composite behind the image.
    pub background_color: Option<String>,

    /// A foreground layer to composite over the image.
    pub foreground_layer: Option<String>,

    /// Whether the encoder must auto-orient the image from EXIF.
    pub requires_auto_orientation: bool,
}

impl Default for ImageProcessingOptions {
    fn default() -> Self {
        Self {
            item_id: Uuid::nil(),
            image: ItemImageInfo::default(),
            image_index: 0,
            width: None,
            height: None,
            max_width: None,
            max_height: None,
            fill_width: None,
            fill_height: None,
            quality: 0,
            supported_output_formats: Vec::new(),
            unplayed_count: None,
            blur: None,
            percent_played: 0.0,
            background_color: None,
            foreground_layer: None,
            requires_auto_orientation: true,
        }
    }
}

impl ImageProcessingOptions {
    /// Whether `original_image_path`'s extension is one of the
    /// [`supported_output_formats`](Self::supported_output_formats), so the
    /// image can be served without re-encoding. Mirrors C# `IsFormatSupported`
    /// (which also folds `.jpeg` onto `.jpg`).
    fn is_format_supported(&self, original_image_path: &str) -> bool {
        let ext = extension_of(original_image_path);
        let ext = if ext.eq_ignore_ascii_case(".jpeg") {
            ".jpg".to_owned()
        } else {
            ext
        };
        self.supported_output_formats
            .iter()
            .any(|f| ext.eq_ignore_ascii_case(f.extension()))
    }

    /// Whether no size-affecting or overlay option is set (the size-independent
    /// half of [`has_default_options`](Self::has_default_options)). Mirrors C#
    /// `HasDefaultOptionsWithoutSize`.
    fn has_default_options_without_size(&self, original_image_path: &str) -> bool {
        self.quality >= DEFAULT_QUALITY_THRESHOLD
            && self.is_format_supported(original_image_path)
            && self.percent_played == 0.0
            && self.unplayed_count.is_none()
            && self.blur.is_none()
            && self.background_color.as_deref().unwrap_or("").is_empty()
            && self.foreground_layer.as_deref().unwrap_or("").is_empty()
    }

    /// Whether the source image can be served untouched: no overlays and, when
    /// `size` is known, the requested dimensions already match the source.
    /// Mirrors the public C# `HasDefaultOptions(path, ImageDimensions?)`.
    #[must_use]
    pub fn has_default_options(
        &self,
        original_image_path: &str,
        size: Option<ImageDimensions>,
    ) -> bool {
        let Some(size) = size else {
            // C# private HasDefaultOptions(path): size-less form also requires
            // that no exact/max/fill dimension is requested. (Fill must be here
            // too — without it a `fillWidth`/`fillHeight` request on an image of
            // unknown stored size is wrongly treated as "no transform" and the
            // uncropped original is served, so a poster renders clipped.)
            return self.has_default_options_without_size(original_image_path)
                && self.width.is_none()
                && self.height.is_none()
                && self.max_width.is_none()
                && self.max_height.is_none()
                && self.fill_width.is_none()
                && self.fill_height.is_none();
        };

        if !self.has_default_options_without_size(original_image_path) {
            return false;
        }

        if self.width.is_some_and(|w| size.width != w) {
            return false;
        }
        if self.height.is_some_and(|h| size.height != h) {
            return false;
        }
        if self.max_width.is_some_and(|mw| size.width > mw) {
            return false;
        }
        if self.max_height.is_some_and(|mh| size.height > mh) {
            return false;
        }
        // C# compares against the raw int fields; an unset FillWidth/Height is 0,
        // so any positive source dimension trips the guard, matching C# exactly.
        if size.width > self.fill_width.unwrap_or(0) || size.height > self.fill_height.unwrap_or(0)
        {
            return false;
        }

        true
    }
}

/// Returns the extension (including the leading dot) of a path, or an empty
/// string when there is none. A small stand-in for C# `Path.GetExtension`.
fn extension_of(path: &str) -> String {
    match path.rsplit_once('.') {
        // Only treat the dot as an extension separator when it is in the final
        // path segment (no `/` or `\` after it).
        Some((_, ext)) if !ext.contains('/') && !ext.contains('\\') => format!(".{ext}"),
        _ => String::new(),
    }
}

/// Options for compositing several images into a single collage.
///
/// Mirrors C# `ImageCollageOptions`. All fields are zero/empty by default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageCollageOptions {
    /// The source image paths to composite.
    pub input_paths: Vec<String>,

    /// The path the collage is written to.
    pub output_path: String,

    /// The collage width in pixels.
    pub width: i32,

    /// The collage height in pixels.
    pub height: i32,
}

#[cfg(test)]
mod tests {
    use super::{ImageCollageOptions, ImageDimensions, ImageFormat, ImageProcessingOptions};
    use uuid::Uuid;

    #[test]
    fn default_requires_auto_orientation() {
        let opts = ImageProcessingOptions::default();
        assert!(opts.requires_auto_orientation);
        assert_eq!(opts.item_id, Uuid::nil());
        assert_eq!(opts.quality, 0);
        assert!(opts.supported_output_formats.is_empty());
    }

    #[test]
    fn low_quality_is_never_default() {
        let opts = ImageProcessingOptions {
            quality: 50,
            supported_output_formats: vec![ImageFormat::Jpg],
            ..Default::default()
        };
        assert!(!opts.has_default_options("/a.jpg", None));
    }

    #[test]
    fn high_quality_supported_format_no_size_is_default() {
        let opts = ImageProcessingOptions {
            quality: 95,
            supported_output_formats: vec![ImageFormat::Jpg],
            ..Default::default()
        };
        assert!(opts.has_default_options("/poster.jpg", None));
        // .jpeg folds onto .jpg.
        assert!(opts.has_default_options("/poster.jpeg", None));
        // Unsupported extension trips the format check.
        assert!(!opts.has_default_options("/poster.png", None));
    }

    #[test]
    fn requested_dimension_breaks_default() {
        let opts = ImageProcessingOptions {
            quality: 95,
            supported_output_formats: vec![ImageFormat::Jpg],
            width: Some(100),
            ..Default::default()
        };
        // No size known: an exact width request means not-default.
        assert!(!opts.has_default_options("/a.jpg", None));
    }

    #[test]
    fn matching_size_within_fill_is_default() {
        let opts = ImageProcessingOptions {
            quality: 95,
            supported_output_formats: vec![ImageFormat::Jpg],
            width: Some(100),
            height: Some(50),
            fill_width: Some(100),
            fill_height: Some(50),
            ..Default::default()
        };
        let size = ImageDimensions::new(100, 50);
        assert!(opts.has_default_options("/a.jpg", Some(size)));

        // A source larger than fill bounds is not default.
        let big = ImageDimensions::new(200, 50);
        assert!(!opts.has_default_options("/a.jpg", Some(big)));
    }

    #[test]
    fn fill_request_is_not_default_without_known_size() {
        // A poster whose stored dimensions are unknown (size = None) requested
        // with fillWidth/fillHeight must NOT be treated as "no transform" — else
        // the uncropped original is served and the poster renders clipped.
        let opts = ImageProcessingOptions {
            quality: 95,
            supported_output_formats: vec![ImageFormat::Jpg],
            fill_width: Some(400),
            fill_height: Some(600),
            ..Default::default()
        };
        assert!(!opts.has_default_options("/poster.jpg", None));
        // With no dimensions of any kind requested, it is default.
        let plain = ImageProcessingOptions {
            quality: 95,
            supported_output_formats: vec![ImageFormat::Jpg],
            ..Default::default()
        };
        assert!(plain.has_default_options("/poster.jpg", None));
    }

    #[test]
    fn collage_default_is_empty() {
        let c = ImageCollageOptions::default();
        assert!(c.input_paths.is_empty());
        assert_eq!(c.width, 0);
        assert_eq!(c.output_path, "");
    }
}
