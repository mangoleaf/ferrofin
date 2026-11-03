//! An image processor that counts what it measures, for the scan tests that
//! pin when a file is decoded.

use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use ferrofin_drawing::ImageProcessor;
use ferrofin_model::drawing::{ImageDimensions, ImageFormat};
use ferrofin_traits::ServiceError;
use ferrofin_traits::drawing::ProcessedImage;
use ferrofin_traits::options::{ImageCollageOptions, ImageProcessingOptions, ItemImageInfo};
use uuid::Uuid;

/// The real image processor, counting the measurements (a size or a
/// blurhash) asked of it.
pub struct CountingImages {
    inner: ImageProcessor,
    measured: AtomicUsize,
}

impl CountingImages {
    /// Counts what `inner` is asked to measure.
    pub fn new(inner: ImageProcessor) -> Self {
        Self {
            inner,
            measured: AtomicUsize::new(0),
        }
    }

    /// How many measurements were asked for so far.
    pub fn measured(&self) -> usize {
        self.measured.load(Ordering::SeqCst)
    }

    fn count(&self) {
        self.measured.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ferrofin_traits::drawing::ImageProcessor for CountingImages {
    fn supported_input_formats(&self) -> Vec<String> {
        self.inner.supported_input_formats()
    }
    fn supports_image_collage_creation(&self) -> bool {
        self.inner.supports_image_collage_creation()
    }
    fn supported_image_output_formats(&self) -> Vec<ImageFormat> {
        self.inner.supported_image_output_formats()
    }
    async fn get_image_dimensions(&self, path: &str) -> Result<ImageDimensions, ServiceError> {
        self.count();
        self.inner.get_image_dimensions(path).await
    }
    async fn get_item_image_dimensions(
        &self,
        item_id: Uuid,
        info: &ItemImageInfo,
    ) -> Result<ImageDimensions, ServiceError> {
        self.count();
        self.inner.get_item_image_dimensions(item_id, info).await
    }
    async fn get_image_blur_hash(&self, path: &str) -> Result<String, ServiceError> {
        self.count();
        self.inner.get_image_blur_hash(path).await
    }
    async fn get_image_blur_hash_sized(
        &self,
        path: &str,
        image_dimensions: ImageDimensions,
    ) -> Result<String, ServiceError> {
        self.count();
        self.inner
            .get_image_blur_hash_sized(path, image_dimensions)
            .await
    }
    async fn get_image_cache_tag(
        &self,
        item_path: Option<&str>,
        image: &ItemImageInfo,
    ) -> Result<Option<String>, ServiceError> {
        self.inner.get_image_cache_tag(item_path, image).await
    }
    async fn get_image_cache_tag_for_path(
        &self,
        base_item_path: &str,
        image_date_modified: DateTime<Utc>,
    ) -> Result<Option<String>, ServiceError> {
        self.inner
            .get_image_cache_tag_for_path(base_item_path, image_date_modified)
            .await
    }
    async fn process_image(
        &self,
        options: &ImageProcessingOptions,
    ) -> Result<ProcessedImage, ServiceError> {
        self.inner.process_image(options).await
    }
    async fn create_image_collage(
        &self,
        options: &ImageCollageOptions,
        library_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.inner.create_image_collage(options, library_name).await
    }
}
