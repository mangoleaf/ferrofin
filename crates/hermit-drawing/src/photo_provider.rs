//! `PhotoProvider` — the photo embedded-information metadata provider, port of
//! `Emby.Photos.PhotoProvider`.
//!
//! In C# this is an `ICustomMetadataProvider<Photo>` + `IForcedProvider` +
//! `IHasItemChangeMonitor`. Two of those strategy interfaces are, per the
//! `hermit-traits` port note, **not** ported as traits — they become
//! match-on-item-kind logic in `hermit-core`; and `IForcedProvider` is a pure
//! marker. So this port is a plain struct exposing the provider's three
//! behaviours as methods:
//!
//! - [`name`](PhotoProvider::name) → the constant `"Embedded Information"`.
//! - [`has_changed`](PhotoProvider::has_changed) → the
//!   `IHasItemChangeMonitor.HasChanged` last-write-time comparison, file-protocol
//!   gated, behind the [`DirectoryService`] seam.
//! - [`fetch`](PhotoProvider::fetch) → the `FetchAsync` body: set the Primary
//!   image path, then backfill zero/negative width/height from the
//!   [`ImageProcessor`](hermit_traits::drawing::ImageProcessor).
//!
//! # Always-on vs. conditional
//!
//! The unit deliberately splits the **always-on** dimension backfill (shipped
//! here) from the **conditional** EXIF metadata mapping (aperture / shutter /
//! make / model / rating / comment / title / date-taken / genres / keywords /
//! software / orientation / exposure / focal length / lat-long-alt / ISO).
//!
//! The C# EXIF block reads into a `Photo` entity via TagLib#. This port would
//! replace TagLib# with `kamadak-exif`, but the mapping requires **both** a
//! `Photo` domain entity to write those fields onto **and** the
//! `ICustomMetadataProvider` provider trait to hang off. Neither exists in
//! `hermit-model` / `hermit-traits` yet (the provider layer is explicitly
//! deferred to `hermit-core` match logic, and there is no `Photo` struct). Per
//! the unit's conditional gate, EXIF is therefore **deferred**: the
//! extension gate ([`is_exif_candidate`]) is ported and unit-tested so the EXIF
//! branch can be dropped in later without reshaping this file, but no
//! `kamadak-exif` dependency is added and no fields are mapped. See the port
//! report / `brain/PLAN_HERMIT_PORT.md`.

use chrono::{DateTime, Utc};
use hermit_model::entities::ImageType;
use hermit_traits::drawing::ImageProcessor;
use hermit_traits::error::ServiceError;
use hermit_traits::options::ItemImageInfo;
use hermit_traits::providers::ItemUpdateType;
use std::sync::Arc;
use uuid::Uuid;

/// The provider's display name. Port of the C# `Name => "Embedded Information"`.
const PROVIDER_NAME: &str = "Embedded Information";

/// The file extensions (leading dot, lowercase) that the EXIF branch is allowed
/// to open.
///
/// Port of the C# `_includeExtensions` set. The C# comment notes the gate
/// exists because "other extensions might cause taglib to hang"; it is kept
/// here (and unit-tested) even though the EXIF branch itself is deferred, so
/// the gate lands byte-for-byte and the later EXIF work only has to fill the
/// branch body. Compared case-insensitively against the source extension.
const EXIF_INCLUDE_EXTENSIONS: [&str; 7] =
    [".jpg", ".jpeg", ".png", ".tiff", ".cr2", ".webp", ".avif"];

/// Whether `path`'s extension is one the (deferred) EXIF branch would open.
///
/// Port of the C# `_includeExtensions.Contains(Path.GetExtension(path),
/// StringComparison.OrdinalIgnoreCase)` gate. Matched case-insensitively; a
/// path with no extension is never a candidate.
#[must_use]
pub fn is_exif_candidate(path: &str) -> bool {
    let ext = extension_of(path);
    EXIF_INCLUDE_EXTENSIONS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&ext))
}

/// The lowercase extension (with leading dot) of `path`, or `""` when there is
/// none. Mirrors C# `Path.GetExtension`, folded to lowercase for the
/// case-insensitive gate.
fn extension_of(path: &str) -> String {
    let last_segment = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match last_segment.rfind('.') {
        Some(idx) => last_segment[idx..].to_ascii_lowercase(),
        None => String::new(),
    }
}

/// A single file's read-only stat, as returned by the directory-service seam.
///
/// Port of the fields of `FileSystemMetadata` that
/// [`PhotoProvider::has_changed`] reads — only the last-write-time is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    /// The file's last-modified time in UTC. Port of `LastWriteTimeUtc`.
    pub last_write_time_utc: DateTime<Utc>,
}

/// The directory-lookup seam [`PhotoProvider::has_changed`] needs.
///
/// Port of the single `IDirectoryService.GetFile(path)` call the C#
/// `HasChanged` makes. Behind a trait so the real filesystem stat stays out of
/// the parity/coverage numbers and unit tests use a fake — matching the crate's
/// [`FileMeta`](crate::processor::FileMeta) convention.
pub trait DirectoryService: Send + Sync {
    /// Returns the file at `path`, or `None` when it does not exist. Port of
    /// `IDirectoryService.GetFile` (which returns `null` for a missing file).
    fn get_file(&self, path: &str) -> Option<FileInfo>;
}

/// The photo whose embedded information is being refreshed.
///
/// A value struct standing in for the C# `Photo` (a `BaseItem` subclass), holding
/// only the fields [`fetch`](PhotoProvider::fetch) and
/// [`has_changed`](PhotoProvider::has_changed) actually read or write. The many
/// EXIF-only fields of the C# `Photo` (aperture, shutter, camera make/model, …)
/// are **omitted** because the EXIF branch is deferred; they arrive with the
/// `Photo` entity in a later unit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhotoItem {
    /// The item's stable identity. Port of `BaseItem.Id`; forwarded to
    /// [`ImageProcessor::get_item_image_dimensions`] (which uses it only for a
    /// debug log line).
    pub id: Uuid,

    /// The path to the source image file. Port of `BaseItem.Path`.
    pub path: String,

    /// Whether the item is backed by a local file (vs. a remote/URL source).
    /// Port of `BaseItem.IsFileProtocol`; gates [`has_changed`](PhotoProvider::has_changed).
    pub is_file_protocol: bool,

    /// The item's recorded last-modified time. Port of `BaseItem.DateModified`,
    /// compared against the on-disk time in [`has_changed`](PhotoProvider::has_changed).
    pub date_modified: DateTime<Utc>,

    /// The image width in pixels (`<= 0` when unknown → backfilled). Port of
    /// `Photo.Width`.
    pub width: i32,

    /// The image height in pixels (`<= 0` when unknown → backfilled). Port of
    /// `Photo.Height`.
    pub height: i32,

    /// The item's attached images, keyed implicitly by [`ItemImageInfo::image_type`].
    /// Port of `BaseItem.ImageInfos`; [`fetch`](PhotoProvider::fetch) sets the
    /// Primary entry and reads it back for dimension probing.
    pub images: Vec<ItemImageInfo>,
}

impl PhotoItem {
    /// Sets the path of the [`ImageType::Primary`] image, inserting the entry if
    /// absent. Port of `BaseItem.SetImagePath(ImageType.Primary, path)`.
    fn set_image_path(&mut self, image_type: ImageType, path: &str) {
        if let Some(existing) = self
            .images
            .iter_mut()
            .find(|info| info.image_type == image_type)
        {
            path.clone_into(&mut existing.path);
        } else {
            self.images.push(ItemImageInfo {
                path: path.to_owned(),
                image_type,
                ..Default::default()
            });
        }
    }

    /// Returns the image row for `image_type` (the C# `GetImageInfo(type, 0)`),
    /// or `None` when the item has no such image.
    fn image_info(&self, image_type: ImageType) -> Option<&ItemImageInfo> {
        self.images
            .iter()
            .find(|info| info.image_type == image_type)
    }
}

/// Metadata provider for photos — the embedded-information provider.
///
/// Port of `Emby.Photos.PhotoProvider`. Holds the shared
/// [`ImageProcessor`](hermit_traits::drawing::ImageProcessor) used to probe
/// image dimensions when the item's stored width/height are unknown.
pub struct PhotoProvider {
    /// The image processor used for dimension probing. Port of the injected
    /// `IImageProcessor`.
    processor: Arc<dyn ImageProcessor>,
}

impl PhotoProvider {
    /// Constructs a [`PhotoProvider`] over the shared image processor. Port of
    /// the C# constructor (the injected `ILogger` is dropped — logging is the
    /// host's concern and has no oracle).
    #[must_use]
    pub fn new(processor: Arc<dyn ImageProcessor>) -> Self {
        Self { processor }
    }

    /// The provider's display name, always `"Embedded Information"`. Port of the
    /// C# `Name` property.
    #[must_use]
    pub fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    /// Whether the item has changed since it was last refreshed. Port of
    /// `IHasItemChangeMonitor.HasChanged`.
    ///
    /// Only file-protocol items are monitored: for them, the on-disk file's
    /// last-write-time is looked up through `directory_service` and compared
    /// against the item's recorded [`date_modified`](PhotoItem::date_modified)
    /// (a strictly-later disk time means changed — the C#
    /// `item.HasChanged(file.LastWriteTimeUtc)` semantics). A non-file-protocol
    /// item, or one whose file is missing, is never "changed".
    #[must_use]
    pub fn has_changed(&self, item: &PhotoItem, directory_service: &dyn DirectoryService) -> bool {
        if !item.is_file_protocol {
            return false;
        }
        match directory_service.get_file(&item.path) {
            Some(file) => file.last_write_time_utc > item.date_modified,
            None => false,
        }
    }

    /// Refreshes the item's embedded information, returning what changed. Port of
    /// `FetchAsync`.
    ///
    /// Always-on behaviour: set the [`ImageType::Primary`] image path to the
    /// item's path, then — when the stored width or height is `<= 0` — probe the
    /// primary image's dimensions through the
    /// [`ImageProcessor`](hermit_traits::drawing::ImageProcessor) and write back
    /// any positive result. A "format not supported" probe error
    /// ([`ServiceError::invalid_input`], the analogue of the C# `catch
    /// (ArgumentException)`) is swallowed, leaving the dimensions untouched;
    /// every other error propagates.
    ///
    /// The EXIF-metadata mapping is deferred (see the module docs), so the C#
    /// `ImageUpdate | MetadataImport` combined result collapses to
    /// [`ItemUpdateType::ImageUpdate`] — the non-`[Flags]`
    /// [`ItemUpdateType`](hermit_traits::providers::ItemUpdateType) port has no
    /// combined variant, and this unit only ever performs an image update.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`ServiceError`] if dimension probing fails for a
    /// reason other than an unsupported format.
    pub async fn fetch(&self, item: &mut PhotoItem) -> Result<ItemUpdateType, ServiceError> {
        let path = item.path.clone();
        item.set_image_path(ImageType::Primary, &path);

        // EXIF branch deferred: the gate is preserved (`is_exif_candidate`) and
        // unit-tested, but there is no `Photo` entity to map onto yet.

        if (item.width <= 0 || item.height <= 0)
            && let Some(info) = item.image_info(ImageType::Primary).cloned()
        {
            match self
                .processor
                .get_item_image_dimensions(item.id, &info)
                .await
            {
                Ok(size) => {
                    if size.width > 0 && size.height > 0 {
                        item.width = size.width;
                        item.height = size.height;
                    }
                }
                // C# `catch (ArgumentException)` — "format not supported".
                Err(ServiceError::InvalidInput(_)) => {}
                Err(other) => return Err(other),
            }
        }

        Ok(ItemUpdateType::ImageUpdate)
    }
}

/// Object-safety assertion for the [`DirectoryService`] seam, matching the
/// crate's trait conventions.
fn _assert_object_safe_directory_service(_: &dyn DirectoryService) {}

#[cfg(test)]
mod tests {
    //! Round-trip cases transliterated from `PhotoProvider.cs`: a zero-width
    //! photo is backfilled from the processor; a photo with positive stored
    //! dimensions is left alone; the primary image path is always set; the EXIF
    //! extension gate accepts the seven photo types and excludes everything
    //! else; `HasChanged` follows the file-protocol + last-write-time rules; and
    //! the reported update type is `ImageUpdate`.
    use super::*;
    use async_trait::async_trait;
    use chrono::TimeZone;
    use hermit_model::drawing::{ImageDimensions, ImageFormat};
    use hermit_traits::drawing::ProcessedImage;
    use hermit_traits::options::{ImageCollageOptions, ImageProcessingOptions};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An [`ImageProcessor`] that reports a fixed size for
    /// `get_item_image_dimensions`, counts probe calls, and can be told to fail
    /// with a given [`ServiceError`] (to exercise the swallow-vs-propagate
    /// branches). Every other trait method is an unused stub.
    struct FakeProcessor {
        size: ImageDimensions,
        calls: AtomicUsize,
        error: Option<fn() -> ServiceError>,
    }

    impl FakeProcessor {
        fn reporting(width: i32, height: i32) -> Self {
            Self {
                size: ImageDimensions::new(width, height),
                calls: AtomicUsize::new(0),
                error: None,
            }
        }

        fn failing(error: fn() -> ServiceError) -> Self {
            Self {
                size: ImageDimensions::default(),
                calls: AtomicUsize::new(0),
                error: Some(error),
            }
        }
    }

    #[async_trait]
    impl ImageProcessor for FakeProcessor {
        fn supported_input_formats(&self) -> Vec<String> {
            vec![]
        }
        fn supports_image_collage_creation(&self) -> bool {
            false
        }
        fn supported_image_output_formats(&self) -> Vec<ImageFormat> {
            vec![]
        }
        async fn get_image_dimensions(&self, _path: &str) -> Result<ImageDimensions, ServiceError> {
            Ok(self.size)
        }
        async fn get_item_image_dimensions(
            &self,
            _item_id: Uuid,
            _info: &ItemImageInfo,
        ) -> Result<ImageDimensions, ServiceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.error {
                Some(make) => Err(make()),
                None => Ok(self.size),
            }
        }
        async fn get_image_blur_hash(&self, _path: &str) -> Result<String, ServiceError> {
            Ok(String::new())
        }
        async fn get_image_blur_hash_sized(
            &self,
            _path: &str,
            _image_dimensions: ImageDimensions,
        ) -> Result<String, ServiceError> {
            Ok(String::new())
        }
        async fn get_image_cache_tag(
            &self,
            _item_id: Uuid,
            _image: &ItemImageInfo,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn get_image_cache_tag_for_path(
            &self,
            _base_item_path: &str,
            _image_date_modified: DateTime<Utc>,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn process_image(
            &self,
            _options: &ImageProcessingOptions,
        ) -> Result<ProcessedImage, ServiceError> {
            Ok(ProcessedImage {
                path: String::new(),
                mime_type: None,
                date_modified: Utc::now(),
            })
        }
        async fn create_image_collage(
            &self,
            _options: &ImageCollageOptions,
            _library_name: Option<&str>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A fake [`DirectoryService`] returning a fixed last-write-time (or nothing,
    /// for a missing file).
    struct FakeDir {
        file: Option<FileInfo>,
    }

    impl DirectoryService for FakeDir {
        fn get_file(&self, _path: &str) -> Option<FileInfo> {
            self.file
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("ts")
    }

    #[tokio::test]
    async fn zero_width_photo_gets_dims_from_processor() {
        let processor = Arc::new(FakeProcessor::reporting(4032, 3024));
        let provider = PhotoProvider::new(processor.clone());
        let mut item = PhotoItem {
            path: "/photos/beach.jpg".into(),
            width: 0,
            height: 0,
            ..Default::default()
        };

        let update = provider.fetch(&mut item).await.expect("fetch");

        assert_eq!(update, ItemUpdateType::ImageUpdate);
        assert_eq!(item.width, 4032);
        assert_eq!(item.height, 3024);
        // Primary image path was set.
        assert_eq!(
            item.image_info(ImageType::Primary).map(|i| i.path.as_str()),
            Some("/photos/beach.jpg")
        );
        assert_eq!(processor.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn positive_dims_are_left_alone_and_not_probed() {
        let processor = Arc::new(FakeProcessor::reporting(4032, 3024));
        let provider = PhotoProvider::new(processor.clone());
        let mut item = PhotoItem {
            path: "/photos/known.jpg".into(),
            width: 800,
            height: 600,
            ..Default::default()
        };

        provider.fetch(&mut item).await.expect("fetch");

        assert_eq!(item.width, 800);
        assert_eq!(item.height, 600);
        // No probe when both dimensions are already positive.
        assert_eq!(processor.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn non_positive_probe_result_leaves_dims_unchanged() {
        // Processor reports 0x0 → no positive dims to write back.
        let processor = Arc::new(FakeProcessor::reporting(0, 0));
        let provider = PhotoProvider::new(processor);
        let mut item = PhotoItem {
            path: "/photos/x.jpg".into(),
            width: 0,
            height: 0,
            ..Default::default()
        };

        provider.fetch(&mut item).await.expect("fetch");

        assert_eq!(item.width, 0);
        assert_eq!(item.height, 0);
    }

    #[tokio::test]
    async fn unsupported_format_probe_error_is_swallowed() {
        // ArgumentException analogue → swallowed, dims untouched, Ok returned.
        let processor = Arc::new(FakeProcessor::failing(|| {
            ServiceError::invalid_input("format not supported")
        }));
        let provider = PhotoProvider::new(processor);
        let mut item = PhotoItem {
            path: "/photos/weird.jpg".into(),
            width: 0,
            height: 0,
            ..Default::default()
        };

        let update = provider.fetch(&mut item).await.expect("fetch swallows");
        assert_eq!(update, ItemUpdateType::ImageUpdate);
        assert_eq!(item.width, 0);
    }

    #[tokio::test]
    async fn other_probe_error_propagates() {
        let processor = Arc::new(FakeProcessor::failing(|| ServiceError::backend("io")));
        let provider = PhotoProvider::new(processor);
        let mut item = PhotoItem {
            path: "/photos/broken.jpg".into(),
            width: 0,
            height: 0,
            ..Default::default()
        };

        let result = provider.fetch(&mut item).await;
        assert!(result.is_err());
    }

    #[test]
    fn ext_gate_accepts_photos_excludes_non_photos() {
        // The seven included extensions, case-insensitively.
        for path in [
            "/a/b.jpg",
            "/a/b.JPEG",
            "photo.png",
            "scan.tiff",
            "raw.CR2",
            "modern.webp",
            "next.avif",
        ] {
            assert!(is_exif_candidate(path), "should accept {path}");
        }
        // Everything else is excluded — including videos, docs, extensionless.
        for path in [
            "/a/movie.mp4",
            "song.mp3",
            "readme.txt",
            "archive.gif",
            "vector.svg",
            "/no/extension",
        ] {
            assert!(!is_exif_candidate(path), "should exclude {path}");
        }
    }

    #[test]
    fn has_changed_true_when_disk_is_newer_for_file_item() {
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(1, 1)));
        let item = PhotoItem {
            path: "/p.jpg".into(),
            is_file_protocol: true,
            date_modified: ts(1_000),
            ..Default::default()
        };
        let dir = FakeDir {
            file: Some(FileInfo {
                last_write_time_utc: ts(2_000),
            }),
        };
        assert!(provider.has_changed(&item, &dir));
    }

    #[test]
    fn has_changed_false_when_disk_not_newer() {
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(1, 1)));
        let item = PhotoItem {
            path: "/p.jpg".into(),
            is_file_protocol: true,
            date_modified: ts(2_000),
            ..Default::default()
        };
        let dir = FakeDir {
            file: Some(FileInfo {
                last_write_time_utc: ts(2_000),
            }),
        };
        assert!(!provider.has_changed(&item, &dir));
    }

    #[test]
    fn has_changed_false_for_non_file_protocol() {
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(1, 1)));
        let item = PhotoItem {
            path: "https://cdn/p.jpg".into(),
            is_file_protocol: false,
            date_modified: ts(0),
            ..Default::default()
        };
        // Even a much-newer disk time is ignored for non-file items.
        let dir = FakeDir {
            file: Some(FileInfo {
                last_write_time_utc: ts(9_999),
            }),
        };
        assert!(!provider.has_changed(&item, &dir));
    }

    #[test]
    fn has_changed_false_when_file_missing() {
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(1, 1)));
        let item = PhotoItem {
            path: "/gone.jpg".into(),
            is_file_protocol: true,
            date_modified: ts(1_000),
            ..Default::default()
        };
        let dir = FakeDir { file: None };
        assert!(!provider.has_changed(&item, &dir));
    }

    #[test]
    fn name_is_embedded_information() {
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(1, 1)));
        assert_eq!(provider.name(), "Embedded Information");
    }
}
