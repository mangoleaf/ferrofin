//! `NullImageEncoder` — the fallback [`ImageEncoder`] that does no real work.
//!
//! Port of `Jellyfin.Drawing.NullImageEncoder`, the `IImageEncoder`
//! implementation the server falls back to when no capable codec is registered.
//! It advertises the input/output format sets (`png`/`jpeg`/`jpg` in;
//! [`ImageFormat::Jpg`]/[`ImageFormat::Png`] out), reports that it cannot
//! actually encode or build collages, and returns an error from every method
//! that would touch pixels.
//!
//! Port rule: the C# `NotImplementedException` thrown by every real method
//! becomes [`ServiceError::Backend`] — the flat error taxonomy's catch-all for
//! infrastructure failures with no more specific variant.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hermit_model::drawing::{ImageDimensions, ImageFormat, ImageOrientation};
use hermit_traits::drawing::ImageEncoder;
use hermit_traits::error::ServiceError;
use hermit_traits::options::{ImageCollageOptions, ImageProcessingOptions};

/// A fallback [`ImageEncoder`] that performs no image work.
///
/// Every method that would decode, encode, or composite pixels returns
/// [`ServiceError::Backend`] (the port of the C# `NotImplementedException`);
/// only the capability accessors return real values.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullImageEncoder;

impl NullImageEncoder {
    /// The message carried by the [`ServiceError::Backend`] every real method
    /// returns, mirroring the C# `NotImplementedException`.
    const NOT_IMPLEMENTED: &'static str = "NullImageEncoder does not implement image processing";

    /// Constructs a new [`NullImageEncoder`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Builds the [`ServiceError::Backend`] returned by every real method.
    fn not_implemented() -> ServiceError {
        ServiceError::backend(Self::NOT_IMPLEMENTED)
    }
}

#[async_trait]
impl ImageEncoder for NullImageEncoder {
    /// Port of `SupportedInputFormats` — the `png`/`jpeg`/`jpg` set.
    fn supported_input_formats(&self) -> Vec<String> {
        vec!["png".to_owned(), "jpeg".to_owned(), "jpg".to_owned()]
    }

    /// Port of `SupportedOutputFormats` — [`ImageFormat::Jpg`] and
    /// [`ImageFormat::Png`].
    fn supported_output_formats(&self) -> Vec<ImageFormat> {
        vec![ImageFormat::Jpg, ImageFormat::Png]
    }

    /// Port of `Name` — the constant `"Null Image Encoder"`.
    fn name(&self) -> String {
        "Null Image Encoder".to_owned()
    }

    /// Port of `SupportsImageCollageCreation` — always `false`.
    fn supports_image_collage_creation(&self) -> bool {
        false
    }

    /// Port of `SupportsImageEncoding` — always `false`.
    fn supports_image_encoding(&self) -> bool {
        false
    }

    /// Port of `GetImageSize` — always [`ServiceError::Backend`].
    async fn get_image_size(&self, _path: &str) -> Result<ImageDimensions, ServiceError> {
        Err(Self::not_implemented())
    }

    /// Port of `GetImageBlurHash` — always [`ServiceError::Backend`].
    async fn get_image_blur_hash(
        &self,
        _x_comp: i32,
        _y_comp: i32,
        _path: &str,
    ) -> Result<String, ServiceError> {
        Err(Self::not_implemented())
    }

    /// Port of `EncodeImage` — always [`ServiceError::Backend`].
    async fn encode_image(
        &self,
        _input_path: &str,
        _date_modified: DateTime<Utc>,
        _output_path: &str,
        _auto_orient: bool,
        _orientation: Option<ImageOrientation>,
        _quality: i32,
        _options: &ImageProcessingOptions,
        _output_format: ImageFormat,
    ) -> Result<String, ServiceError> {
        Err(Self::not_implemented())
    }

    /// Port of `CreateImageCollage` — always [`ServiceError::Backend`].
    async fn create_image_collage(
        &self,
        _options: &ImageCollageOptions,
        _library_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        Err(Self::not_implemented())
    }

    /// Port of `CreateSplashscreen` — always [`ServiceError::Backend`].
    async fn create_splashscreen(
        &self,
        _posters: &[String],
        _backdrops: &[String],
    ) -> Result<(), ServiceError> {
        Err(Self::not_implemented())
    }

    /// Port of `CreateTrickplayTile` — always [`ServiceError::Backend`].
    async fn create_trickplay_tile(
        &self,
        _options: &ImageCollageOptions,
        _quality: i32,
        _img_width: i32,
        _img_height: Option<i32>,
    ) -> Result<i32, ServiceError> {
        Err(Self::not_implemented())
    }
}

#[cfg(test)]
mod tests {
    //! Transliterated from `NullImageEncoder.cs`: the capability accessors are
    //! the oracle for the advertised format sets, and every real method throws
    //! `NotImplementedException` (here [`ServiceError::Backend`]).
    use super::*;

    /// Builds the `ImageProcessingOptions`/`ImageCollageOptions` needed to call
    /// the real methods. Defaults suffice — the encoder never inspects them.
    fn processing_options() -> ImageProcessingOptions {
        ImageProcessingOptions::default()
    }

    fn collage_options() -> ImageCollageOptions {
        ImageCollageOptions::default()
    }

    #[test]
    fn advertises_supported_input_formats() {
        let encoder = NullImageEncoder::new();
        assert_eq!(
            encoder.supported_input_formats(),
            vec!["png".to_owned(), "jpeg".to_owned(), "jpg".to_owned()]
        );
    }

    #[test]
    fn advertises_supported_output_formats() {
        let encoder = NullImageEncoder::new();
        assert_eq!(
            encoder.supported_output_formats(),
            vec![ImageFormat::Jpg, ImageFormat::Png]
        );
    }

    #[test]
    fn advertises_name() {
        assert_eq!(NullImageEncoder::new().name(), "Null Image Encoder");
    }

    #[test]
    fn does_not_support_collage_creation() {
        assert!(!NullImageEncoder::new().supports_image_collage_creation());
    }

    #[test]
    fn does_not_support_image_encoding() {
        assert!(!NullImageEncoder::new().supports_image_encoding());
    }

    #[tokio::test]
    async fn get_image_size_errors() {
        let encoder = NullImageEncoder::new();
        assert!(matches!(
            encoder.get_image_size("/tmp/a.png").await,
            Err(ServiceError::Backend(_))
        ));
    }

    #[tokio::test]
    async fn get_image_blur_hash_errors() {
        let encoder = NullImageEncoder::new();
        assert!(matches!(
            encoder.get_image_blur_hash(3, 3, "/tmp/a.png").await,
            Err(ServiceError::Backend(_))
        ));
    }

    #[tokio::test]
    async fn encode_image_errors() {
        let encoder = NullImageEncoder::new();
        let options = processing_options();
        let result = encoder
            .encode_image(
                "/tmp/in.png",
                Utc::now(),
                "/tmp/out.jpg",
                false,
                None,
                90,
                &options,
                ImageFormat::Jpg,
            )
            .await;
        assert!(matches!(result, Err(ServiceError::Backend(_))));
    }

    #[tokio::test]
    async fn create_image_collage_errors() {
        let encoder = NullImageEncoder::new();
        let options = collage_options();
        assert!(matches!(
            encoder.create_image_collage(&options, None).await,
            Err(ServiceError::Backend(_))
        ));
    }

    #[tokio::test]
    async fn create_splashscreen_errors() {
        let encoder = NullImageEncoder::new();
        assert!(matches!(
            encoder.create_splashscreen(&[], &[]).await,
            Err(ServiceError::Backend(_))
        ));
    }

    #[tokio::test]
    async fn create_trickplay_tile_errors() {
        let encoder = NullImageEncoder::new();
        let options = collage_options();
        assert!(matches!(
            encoder
                .create_trickplay_tile(&options, 90, 10, Some(10))
                .await,
            Err(ServiceError::Backend(_))
        ));
    }
}
