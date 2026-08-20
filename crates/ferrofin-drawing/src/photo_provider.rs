//! `PhotoProvider` — the photo embedded-information metadata provider, port of
//! `Emby.Photos.PhotoProvider`.
//!
//! In C# this is an `ICustomMetadataProvider<Photo>` + `IForcedProvider` +
//! `IHasItemChangeMonitor`. Two of those strategy interfaces are, per the
//! `ferrofin-traits` port note, **not** ported as traits — they become
//! match-on-item-kind logic in `ferrofin-core`; and `IForcedProvider` is a pure
//! marker. So this port is a plain struct exposing the provider's three
//! behaviours as methods:
//!
//! - [`name`](PhotoProvider::name) → the constant `"Embedded Information"`.
//! - [`has_changed`](PhotoProvider::has_changed) → the
//!   `IHasItemChangeMonitor.HasChanged` last-write-time comparison, file-protocol
//!   gated, behind the [`DirectoryService`] seam.
//! - [`fetch`](PhotoProvider::fetch) → the `FetchAsync` body: set the Primary
//!   image path, then backfill zero/negative width/height from the
//!   [`ImageProcessor`](ferrofin_traits::drawing::ImageProcessor).
//!
//! # Always-on vs. conditional
//!
//! Two halves, as in C#: the **always-on** dimension backfill, and the
//! **conditional** EXIF mapping behind the extension gate
//! ([`is_exif_candidate`]) — aperture / shutter / make / model / rating /
//! comment / title / date-taken / software / orientation / exposure / focal
//! length / lat-long-alt / ISO.
//!
//! C# reads the tags through TagLib#; this port uses `kamadak-exif`, which
//! reads the same TIFF/EXIF IFDs. Two upstream fields have no EXIF source and
//! are therefore not mapped: `Genres` and `Tags` come from TagLib#'s XMP/IPTC
//! keyword aggregation, which `kamadak-exif` does not parse.

use chrono::{DateTime, Datelike as _, NaiveDateTime, TimeZone as _, Utc};
use ferrofin_model::drawing::ImageOrientation;
use ferrofin_model::entities::ImageType;
use ferrofin_traits::drawing::ImageProcessor;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::providers::ItemUpdateType;
use std::sync::Arc;
use uuid::Uuid;

/// The provider's display name. Port of the C# `Name => "Embedded Information"`.
const PROVIDER_NAME: &str = "Embedded Information";

/// The file extensions (leading dot, lowercase) that the EXIF branch is allowed
/// to open.
///
/// Port of the C# `_includeExtensions` set. The C# comment notes the gate
/// exists because "other extensions might cause taglib to hang". Compared
/// case-insensitively against the source extension.
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
/// the fields [`fetch`](PhotoProvider::fetch) and
/// [`has_changed`](PhotoProvider::has_changed) read or write.
#[derive(Debug, Clone, PartialEq, Default)]
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

    /// The EXIF fields read off the file. Port of the `Photo` subclass's
    /// EXIF-only properties, filled by [`fetch`](PhotoProvider::fetch).
    pub exif: PhotoExif,

    /// `BaseItem.Name` — replaced by the EXIF title when the file carries one
    /// and the field is not locked.
    pub name: Option<String>,

    /// `BaseItem.Overview` — the EXIF user comment.
    pub overview: Option<String>,

    /// `BaseItem.CommunityRating` — the EXIF rating.
    pub community_rating: Option<f64>,

    /// `BaseItem.PremiereDate` / `DateCreated`, from the EXIF date-taken.
    pub date_taken: Option<DateTime<Utc>>,

    /// `BaseItem.ProductionYear`, the year of [`date_taken`](Self::date_taken).
    pub production_year: Option<i32>,

    /// Whether `Name` is admin-locked. Port of
    /// `item.LockedFields.Contains(MetadataField.Name)`, which suppresses the
    /// EXIF title.
    pub name_locked: bool,
}

/// The EXIF-only fields of the C# `Photo`, in the order `FetchAsync` sets them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PhotoExif {
    /// `Photo.Aperture` — the EXIF `ApertureValue` rational.
    pub aperture: Option<f64>,
    /// `Photo.ShutterSpeed` — the EXIF `ShutterSpeedValue` rational.
    pub shutter_speed: Option<f64>,
    /// `Photo.CameraMake` — TIFF `Make`.
    pub camera_make: Option<String>,
    /// `Photo.CameraModel` — TIFF `Model`.
    pub camera_model: Option<String>,
    /// `Photo.Software` — TIFF `Software`.
    pub software: Option<String>,
    /// `Photo.Orientation` — TIFF `Orientation`, `None` for the unset value.
    pub orientation: Option<ImageOrientation>,
    /// `Photo.ExposureTime` — the EXIF `ExposureTime` rational, in seconds.
    pub exposure_time: Option<f64>,
    /// `Photo.FocalLength` — the EXIF `FocalLength` rational, in millimetres.
    pub focal_length: Option<f64>,
    /// `Photo.Latitude` — GPS latitude as signed decimal degrees.
    pub latitude: Option<f64>,
    /// `Photo.Longitude` — GPS longitude as signed decimal degrees.
    pub longitude: Option<f64>,
    /// `Photo.Altitude` — GPS altitude in metres (negative below sea level).
    pub altitude: Option<f64>,
    /// `Photo.IsoSpeedRating` — the EXIF `PhotographicSensitivity`.
    pub iso_speed_rating: Option<i32>,
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
/// [`ImageProcessor`](ferrofin_traits::drawing::ImageProcessor) used to probe
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
    /// [`ImageProcessor`](ferrofin_traits::drawing::ImageProcessor) and write back
    /// any positive result. A "format not supported" probe error
    /// ([`ServiceError::invalid_input`], the analogue of the C# `catch
    /// (ArgumentException)`) is swallowed, leaving the dimensions untouched;
    /// every other error propagates.
    ///
    /// The C# `ImageUpdate | MetadataImport` combined result collapses to
    /// [`ItemUpdateType::ImageUpdate`] — the non-`[Flags]`
    /// [`ItemUpdateType`](ferrofin_traits::providers::ItemUpdateType) port has no
    /// combined variant.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`ServiceError`] if dimension probing fails for a
    /// reason other than an unsupported format.
    pub async fn fetch(&self, item: &mut PhotoItem) -> Result<ItemUpdateType, ServiceError> {
        let path = item.path.clone();
        item.set_image_path(ImageType::Primary, &path);

        // The EXIF branch, gated on the extension set (C#: "other extensions
        // might cause taglib to hang"). A file that cannot be read or carries no
        // EXIF simply leaves the fields alone — C# catches and logs.
        if is_exif_candidate(&path)
            && let Some(tags) = read_exif(&path)
        {
            apply_exif(item, &tags);
        }

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

/// The EXIF fields read off one file, plus the image dimensions the tags carry.
///
/// A flat intermediate so the tag→item mapping ([`apply_exif`]) stays pure and
/// unit-testable without a file on disk.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExifTags {
    /// The EXIF-only fields.
    pub exif: PhotoExif,
    /// `ImageDescription`/`UserComment` — C# `ImageTag.Comment`.
    pub comment: Option<String>,
    /// `XPTitle`/`ImageDescription` — C# `ImageTag.Title`.
    pub title: Option<String>,
    /// `Rating` — C# `ImageTag.Rating`, 0–5.
    pub rating: Option<f64>,
    /// `DateTimeOriginal` (falling back to `DateTime`) — C# `ImageTag.DateTime`.
    pub date_taken: Option<DateTime<Utc>>,
    /// `PixelXDimension` — C# `Properties.PhotoWidth`.
    pub width: Option<i32>,
    /// `PixelYDimension` — C# `Properties.PhotoHeight`.
    pub height: Option<i32>,
}

/// Reads `path`'s EXIF tags, or `None` when the file cannot be opened or holds
/// no EXIF. Port of the C# `TagLib.File.Create` + `catch (Exception)` block.
fn read_exif(path: &str) -> Option<ExifTags> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    // A read failure is not worth failing a refresh over: most photos in a
    // library carry no EXIF at all, which is exactly this error.
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    Some(tags_from(&exif))
}

/// Projects an [`exif::Exif`] onto [`ExifTags`]. Pure — the seam the mapping
/// tests drive with a synthetic in-memory image.
fn tags_from(exif: &exif::Exif) -> ExifTags {
    use exif::{In, Tag};

    let rational = |tag: Tag| -> Option<f64> {
        match exif.get_field(tag, In::PRIMARY)?.value {
            exif::Value::Rational(ref v) => v.first().map(exif::Rational::to_f64),
            exif::Value::SRational(ref v) => v.first().map(exif::SRational::to_f64),
            _ => None,
        }
    };
    // Read the raw bytes, never `display_value()`: that renders an ASCII field
    // quoted with every non-ASCII byte escaped (`café` → `caf\xc3\xa9`), and
    // renders an unknown BYTE field as a comma-separated list of numbers.
    let text = |tag: Tag| -> Option<String> {
        let field = exif.get_field(tag, In::PRIMARY)?;
        let value = match &field.value {
            exif::Value::Ascii(parts) => {
                let bytes: Vec<u8> = parts.concat();
                String::from_utf8_lossy(&bytes).into_owned()
            }
            // The Windows XP* tags are BYTE arrays holding UTF-16LE.
            exif::Value::Byte(bytes) => utf16le(bytes),
            exif::Value::Undefined(bytes, _) => String::from_utf8_lossy(bytes).into_owned(),
            _ => return None,
        };
        let value = value.trim_end_matches('\0').trim();
        (!value.is_empty()).then(|| value.to_owned())
    };
    let integer = |tag: Tag| -> Option<i32> {
        exif.get_field(tag, In::PRIMARY)?
            .value
            .get_uint(0)
            .and_then(|v| i32::try_from(v).ok())
    };

    ExifTags {
        exif: PhotoExif {
            aperture: rational(Tag::ApertureValue),
            shutter_speed: rational(Tag::ShutterSpeedValue),
            camera_make: text(Tag::Make),
            camera_model: text(Tag::Model),
            software: text(Tag::Software),
            orientation: integer(Tag::Orientation).and_then(orientation_from),
            exposure_time: rational(Tag::ExposureTime),
            focal_length: rational(Tag::FocalLength),
            latitude: gps_degrees(exif, Tag::GPSLatitude, Tag::GPSLatitudeRef, 'S'),
            longitude: gps_degrees(exif, Tag::GPSLongitude, Tag::GPSLongitudeRef, 'W'),
            altitude: gps_altitude(exif),
            iso_speed_rating: integer(Tag::PhotographicSensitivity),
        },
        // TagLib's `ImageTag.Comment` prefers the standard EXIF UserComment and
        // only falls back to the TIFF ImageDescription.
        comment: text(Tag::UserComment).or_else(|| text(Tag::ImageDescription)),
        // XPTitle (0x9c9b) and Rating (0x4746) are Windows' TIFF extensions —
        // TagLib# surfaces both as ImageTag.Title/Rating, kamadak-exif has no
        // named constant for either.
        // Only a real title becomes the item name. Falling back to the
        // description here would set Name == Overview for every photo that
        // carries only a description.
        title: text(XP_TITLE),
        rating: integer(RATING).map(f64::from),
        date_taken: exif_datetime(exif),
        width: integer(Tag::PixelXDimension),
        height: integer(Tag::PixelYDimension),
    }
}

/// Decodes a UTF-16LE byte run, as the Windows `XP*` TIFF tags store text.
fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// The Windows `XPTitle` TIFF tag (`0x9c9b`).
const XP_TITLE: exif::Tag = exif::Tag(exif::Context::Tiff, 0x9c9b);
/// The Windows `Rating` TIFF tag (`0x4746`), 0–5.
const RATING: exif::Tag = exif::Tag(exif::Context::Tiff, 0x4746);

/// The TIFF orientation value as an [`ImageOrientation`]. `0` (the "None" value
/// C# maps to `null`) and anything out of range yield `None`.
fn orientation_from(value: i32) -> Option<ImageOrientation> {
    Some(match value {
        1 => ImageOrientation::TopLeft,
        2 => ImageOrientation::TopRight,
        3 => ImageOrientation::BottomRight,
        4 => ImageOrientation::BottomLeft,
        5 => ImageOrientation::LeftTop,
        6 => ImageOrientation::RightTop,
        7 => ImageOrientation::RightBottom,
        8 => ImageOrientation::LeftBottom,
        _ => return None,
    })
}

/// A GPS coordinate as signed decimal degrees: EXIF stores it as a
/// degrees/minutes/seconds triple plus a hemisphere reference letter.
fn gps_degrees(
    exif: &exif::Exif,
    tag: exif::Tag,
    ref_tag: exif::Tag,
    negative_ref: char,
) -> Option<f64> {
    let field = exif.get_field(tag, exif::In::PRIMARY)?;
    let exif::Value::Rational(ref dms) = field.value else {
        return None;
    };
    let [d, m, sec] = dms.get(..3)? else {
        return None;
    };
    let degrees = d.to_f64() + m.to_f64() / 60.0 + sec.to_f64() / 3600.0;
    let hemisphere = exif
        .get_field(ref_tag, exif::In::PRIMARY)
        .map(|f| f.display_value().to_string());
    let negative = hemisphere
        .as_deref()
        .is_some_and(|h| h.trim().starts_with(negative_ref));
    Some(if negative { -degrees } else { degrees })
}

/// GPS altitude in metres; `GPSAltitudeRef == 1` means below sea level.
fn gps_altitude(exif: &exif::Exif) -> Option<f64> {
    let field = exif.get_field(exif::Tag::GPSAltitude, exif::In::PRIMARY)?;
    let exif::Value::Rational(ref v) = field.value else {
        return None;
    };
    let metres = v.first()?.to_f64();
    let below = exif
        .get_field(exif::Tag::GPSAltitudeRef, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        == Some(1);
    Some(if below { -metres } else { metres })
}

/// The date the photo was taken: `DateTimeOriginal`, else `DateTime`. EXIF
/// stores local time with no zone, which C# also treats as local — Ferrofin
/// stamps it as UTC so the value round-trips unchanged.
fn exif_datetime(exif: &exif::Exif) -> Option<DateTime<Utc>> {
    for tag in [exif::Tag::DateTimeOriginal, exif::Tag::DateTime] {
        let Some(field) = exif.get_field(tag, exif::In::PRIMARY) else {
            continue;
        };
        let raw = field.display_value().to_string();
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S") {
            return Utc.from_utc_datetime(&naive).into();
        }
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw.trim(), "%Y:%m:%d %H:%M:%S") {
            return Utc.from_utc_datetime(&naive).into();
        }
    }
    None
}

/// Writes the read tags onto the item, in the order C# `FetchAsync` does.
///
/// Every assignment is unconditional (as upstream's is) except the title, which
/// C# skips for a locked `Name`, and the dimensions, which are only taken when
/// the tags carry positive ones.
fn apply_exif(item: &mut PhotoItem, tags: &ExifTags) {
    item.exif = tags.exif.clone();
    if let Some(width) = tags.width.filter(|w| *w > 0) {
        item.width = width;
    }
    if let Some(height) = tags.height.filter(|h| *h > 0) {
        item.height = height;
    }
    if tags.rating.is_some() {
        item.community_rating = tags.rating;
    }
    if tags.comment.is_some() {
        item.overview.clone_from(&tags.comment);
    }
    if let Some(title) = tags.title.as_deref().filter(|t| !t.trim().is_empty())
        && !item.name_locked
    {
        item.name = Some(title.to_owned());
    }
    if let Some(taken) = tags.date_taken {
        item.date_taken = Some(taken);
        item.production_year = Some(taken.year());
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
    use ferrofin_model::drawing::{ImageDimensions, ImageFormat};
    use ferrofin_traits::drawing::ProcessedImage;
    use ferrofin_traits::options::{ImageCollageOptions, ImageProcessingOptions};
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

    /// A minimal but real JPEG carrying an EXIF APP1 segment built from
    /// `entries` — `(tag, type, count, value-or-offset)` little-endian IFD0
    /// rows plus the raw bytes any of them point at.
    ///
    /// Hand-assembled rather than pulled from a fixture file so the expected
    /// values are visible in the test and no binary lands in the repo.
    fn jpeg_with_exif(
        entries: &[(u16, u16, u32, [u8; 4])],
        extra: &[u8],
        extra_base: u32,
    ) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II\x2a\x00"); // little-endian TIFF header
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
        tiff.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
        for (tag, kind, count, value) in entries {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&kind.to_le_bytes());
            tiff.extend_from_slice(&count.to_le_bytes());
            tiff.extend_from_slice(value);
        }
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no IFD1
        assert_eq!(
            u32::try_from(tiff.len()).unwrap(),
            extra_base,
            "extra_base must be where the out-of-line bytes actually start"
        );
        tiff.extend_from_slice(extra);

        let mut app1 = Vec::from(b"Exif\x00\x00".as_slice());
        app1.extend_from_slice(&tiff);

        let mut jpeg = Vec::from(b"\xff\xd8".as_slice()); // SOI
        jpeg.extend_from_slice(b"\xff\xe1");
        jpeg.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap().to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(b"\xff\xd9"); // EOI
        jpeg
    }

    /// Writes `bytes` to a temp file with `name` and returns the path.
    fn write_temp(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write");
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn fetch_reads_camera_make_model_and_orientation_from_a_real_file() {
        // IFD0 with Make ("ACME\0", 5 ASCII bytes, out of line), Model ("X1\0"
        // inline) and Orientation = 6 (RightTop).
        let entries = [
            (0x010f, 2u16, 5u32, 50u32.to_le_bytes()), // Make -> offset 50
            (0x0110, 2, 3, *b"X1\0\0"),                // Model, inline
            (0x0112, 3, 1, 6u32.to_le_bytes()),        // Orientation = 6
        ];
        let jpeg = jpeg_with_exif(&entries, b"ACME\0", 50);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_temp(&dir, "photo.jpg", &jpeg);

        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(0, 0)));
        let mut item = PhotoItem {
            path: path.clone(),
            width: 100,
            height: 100,
            ..Default::default()
        };
        provider.fetch(&mut item).await.expect("fetch");

        assert_eq!(item.exif.camera_make.as_deref(), Some("ACME"));
        assert_eq!(item.exif.camera_model.as_deref(), Some("X1"));
        assert_eq!(item.exif.orientation, Some(ImageOrientation::RightTop));
        // The Primary image path is still set, as before.
        assert_eq!(
            item.image_info(ImageType::Primary).map(|i| i.path.as_str()),
            Some(path.as_str())
        );
    }

    #[tokio::test]
    async fn a_file_without_exif_leaves_every_field_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Two bytes of nothing: openable, but no EXIF container.
        let path = write_temp(&dir, "empty.jpg", b"\xff\xd8");
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(4, 3)));
        let mut item = PhotoItem {
            path,
            ..Default::default()
        };
        provider.fetch(&mut item).await.expect("fetch");
        assert_eq!(item.exif, PhotoExif::default());
        // The always-on dimension backfill still runs.
        assert_eq!((item.width, item.height), (4, 3));
    }

    #[tokio::test]
    async fn a_non_candidate_extension_is_never_opened_for_exif() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A real EXIF payload, but under an extension the gate excludes.
        let entries = [(0x010f, 2u16, 5u32, 26u32.to_le_bytes())];
        let jpeg = jpeg_with_exif(&entries, b"ACME\0", 26);
        let path = write_temp(&dir, "photo.heic", &jpeg);
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(1, 1)));
        let mut item = PhotoItem {
            path,
            width: 1,
            height: 1,
            ..Default::default()
        };
        provider.fetch(&mut item).await.expect("fetch");
        assert_eq!(item.exif.camera_make, None);
    }

    #[tokio::test]
    async fn text_fields_decode_utf8_and_utf16_rather_than_debug_rendering() {
        // Two real hazards: an ASCII field holding UTF-8 bytes (`display_value`
        // escapes them), and the Windows XPTitle BYTE array holding UTF-16LE
        // (`display_value` renders it as a list of numbers).
        let make = "café".as_bytes();
        let xp_title: Vec<u8> = "Sunset"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .chain([0, 0])
            .collect();
        let entries = [
            (0x010f, 2u16, u32::try_from(make.len()).unwrap(), 0u32),
            (0x9c9b, 1, u32::try_from(xp_title.len()).unwrap(), 0),
        ];
        let base = 8 + 2 + entries.len() * 12 + 4;
        let mut extra = Vec::new();
        extra.extend_from_slice(make);
        let xp_offset = base + extra.len();
        extra.extend_from_slice(&xp_title);
        let entries = [
            (
                0x010f,
                2u16,
                u32::try_from(make.len()).unwrap(),
                u32::try_from(base).unwrap().to_le_bytes(),
            ),
            (
                0x9c9b,
                1,
                u32::try_from(xp_title.len()).unwrap(),
                u32::try_from(xp_offset).unwrap().to_le_bytes(),
            ),
        ];
        let jpeg = jpeg_with_exif(&entries, &extra, u32::try_from(base).unwrap());

        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_temp(&dir, "photo.jpg", &jpeg);
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(0, 0)));
        let mut item = PhotoItem {
            path,
            width: 1,
            height: 1,
            ..Default::default()
        };
        provider.fetch(&mut item).await.expect("fetch");

        assert_eq!(
            item.exif.camera_make.as_deref(),
            Some("café"),
            "a UTF-8 ASCII field must not come back escaped"
        );
        assert_eq!(
            item.name.as_deref(),
            Some("Sunset"),
            "XPTitle is UTF-16LE, not a list of byte values"
        );
    }

    #[tokio::test]
    async fn a_description_only_photo_keeps_its_filename_derived_name() {
        // ImageDescription is a comment, not a title: it must fill Overview
        // without overwriting the name.
        let description = b"On the pier";
        let entries = [(
            0x010e,
            2u16,
            u32::try_from(description.len()).unwrap(),
            50u32.to_le_bytes(),
        )];
        let jpeg = jpeg_with_exif(&entries, description, 26);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_temp(&dir, "photo.jpg", &jpeg);
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(0, 0)));
        let mut item = PhotoItem {
            path,
            width: 1,
            height: 1,
            name: Some("DSC_0001".into()),
            ..Default::default()
        };
        provider.fetch(&mut item).await.expect("fetch");
        assert_eq!(item.name.as_deref(), Some("DSC_0001"));
    }

    #[test]
    fn orientation_zero_and_out_of_range_are_none() {
        // C# maps TagLib's `ImageOrientation.None` to a null Orientation.
        assert_eq!(orientation_from(0), None);
        assert_eq!(orientation_from(9), None);
        assert_eq!(orientation_from(1), Some(ImageOrientation::TopLeft));
        assert_eq!(orientation_from(8), Some(ImageOrientation::LeftBottom));
    }

    #[test]
    fn apply_exif_writes_title_rating_comment_and_date() {
        let tags = ExifTags {
            title: Some("Sunset".into()),
            comment: Some("On the pier".into()),
            rating: Some(4.0),
            date_taken: Some(ts(1_600_000_000)),
            width: Some(4032),
            height: Some(3024),
            ..ExifTags::default()
        };
        let mut item = PhotoItem::default();
        apply_exif(&mut item, &tags);
        assert_eq!(item.name.as_deref(), Some("Sunset"));
        assert_eq!(item.overview.as_deref(), Some("On the pier"));
        assert_eq!(item.community_rating, Some(4.0));
        assert_eq!(item.date_taken, Some(ts(1_600_000_000)));
        assert_eq!(item.production_year, Some(2020));
        assert_eq!((item.width, item.height), (4032, 3024));
    }

    #[test]
    fn a_locked_name_keeps_its_value() {
        // C# skips the EXIF title when MetadataField.Name is locked.
        let tags = ExifTags {
            title: Some("Sunset".into()),
            ..ExifTags::default()
        };
        let mut item = PhotoItem {
            name: Some("Curated name".into()),
            name_locked: true,
            ..Default::default()
        };
        apply_exif(&mut item, &tags);
        assert_eq!(item.name.as_deref(), Some("Curated name"));
    }

    #[test]
    fn zero_dimensions_in_the_tags_do_not_overwrite_known_ones() {
        let tags = ExifTags {
            width: Some(0),
            height: Some(0),
            ..ExifTags::default()
        };
        let mut item = PhotoItem {
            width: 800,
            height: 600,
            ..Default::default()
        };
        apply_exif(&mut item, &tags);
        assert_eq!((item.width, item.height), (800, 600));
    }
}
