//! `PhotoProvider` — the photo embedded-information metadata provider, port of
//! `Emby.Photos.PhotoProvider`.
//!
//! In C# this is an `ICustomMetadataProvider<Photo>` + `IForcedProvider` +
//! `IHasItemChangeMonitor`. Two of those strategy interfaces are, per the
//! `ferrofin-traits` port note, **not** ported as traits — they become
//! match-on-item-kind logic in `ferrofin-core`; and `IForcedProvider` is a pure
//! marker. `IHasItemChangeMonitor.HasChanged` (run the provider when the
//! file's last write time moved) belongs to the refresh decision in
//! `ferrofin_providers::refresh_plan`, which covers a changed photo through
//! `RequiresRefresh` in `Default` and `FullRefresh` refreshes; a
//! `ValidationOnly` refresh and a row with no stored `DateModified` do not run
//! this provider yet (gl-1530). So this port is a plain struct exposing the
//! provider's two behaviours as methods:
//!
//! - [`name`](PhotoProvider::name) → the constant `"Embedded Information"`.
//! - [`fetch`](PhotoProvider::fetch) → the `FetchAsync` body: set the Primary
//!   image path, then backfill zero/negative width/height from the
//!   [`ImageProcessor`].
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
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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

/// Whether `path`'s extension is one the EXIF branch opens.
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

/// The photo whose embedded information is being refreshed.
///
/// A value struct standing in for the C# `Photo` (a `BaseItem` subclass), holding
/// the fields [`fetch`](PhotoProvider::fetch) reads or writes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PhotoItem {
    /// The item's stable identity. Port of `BaseItem.Id`; forwarded to
    /// [`ImageProcessor::get_item_image_dimensions`] (which uses it only for a
    /// debug log line).
    pub id: Uuid,

    /// The path to the source image file. Port of `BaseItem.Path`.
    pub path: String,

    /// Set by [`PhotoProvider::fetch`] when the file was read as TagLib would
    /// open it, with or without EXIF, and for a WebP or an AVIF Ferrofin reads
    /// (until gl-1614 decides).
    ///
    /// Not a Jellyfin field: C# expresses the same thing by only entering the
    /// assignment block inside its `try`. A caller must not write the
    /// unconditionally-assigned fields (`overview`, `community_rating`) back
    /// onto a row when this is `false`, or a file TagLib would refuse would
    /// clear values it never spoke about.
    pub exif_was_read: bool,

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

    /// `BaseItem.Overview` — TagLib#'s combined comment, "" when the file holds
    /// none (see `read_exif`).
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
    /// Points the `image_type` image at the file `path`, last written at
    /// `date_modified`, inserting the entry if absent. Port of
    /// `BaseItem.SetImagePath(ImageType, int, FileSystemMetadata)`: an
    /// existing image has its size reset, which leaves the file to be
    /// measured again, and keeps its blurhash until that happens.
    ///
    /// The `http` arm of the C# string overload has no caller here: the scan's
    /// photos are files.
    fn set_image_path(&mut self, image_type: ImageType, path: &str, date_modified: DateTime<Utc>) {
        if let Some(existing) = self
            .images
            .iter_mut()
            .find(|info| info.image_type == image_type)
        {
            path.clone_into(&mut existing.path);
            existing.date_modified = date_modified;
            // "reset these values"
            existing.width = 0;
            existing.height = 0;
        } else {
            self.images.push(ItemImageInfo {
                path: path.to_owned(),
                image_type,
                date_modified,
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

/// `path`'s last write time: the value `file_date_modified` in
/// `ferrofin-providers` stores for every other local image, the current time
/// when the file cannot be read included, so the scan's
/// `image_metadata_is_current` compares like with like.
fn last_write_time(path: &str) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_or_else(|_| Utc::now(), DateTime::<Utc>::from)
}

/// Metadata provider for photos — the embedded-information provider.
///
/// Port of `Emby.Photos.PhotoProvider`. Holds the shared
/// [`ImageProcessor`] used to probe
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

    /// Refreshes the item's embedded information, returning what changed. Port of
    /// `FetchAsync`.
    ///
    /// Always-on behaviour: point the [`ImageType::Primary`] image at the
    /// item's file, dated by its last write time and with an existing image's
    /// size reset, then — when the stored width or height is `<= 0` — probe the
    /// primary image's dimensions through the
    /// [`ImageProcessor`] and write back
    /// any positive result. A "format not supported" probe error
    /// ([`ServiceError::invalid_input`], the analogue of the C# `catch
    /// (ArgumentException)`) is swallowed, leaving the dimensions untouched;
    /// every other error propagates.
    ///
    /// The C# `ImageUpdate | MetadataImport` combined result collapses to
    /// [`ItemUpdateType::ImageUpdate`] — the non-`[Flags]`
    /// [`ItemUpdateType`] port has no
    /// combined variant.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`ServiceError`] if dimension probing fails for a
    /// reason other than an unsupported format.
    pub async fn fetch(&self, item: &mut PhotoItem) -> Result<ItemUpdateType, ServiceError> {
        let path = item.path.clone();
        // On the blocking pool like the EXIF read below: a stalled mount holds
        // a stat as long as it holds an open.
        let owned = path.clone();
        let modified = tokio::task::spawn_blocking(move || last_write_time(&owned))
            .await
            .unwrap_or_else(|_| Utc::now());
        item.set_image_path(ImageType::Primary, &path, modified);

        // The EXIF branch, gated on the extension set (C#: "other extensions
        // might cause taglib to hang"). A file TagLib would fail to open leaves
        // the fields alone — C# catches and logs (see `read_exif`).
        if is_exif_candidate(&path) {
            // Opening and parsing the container is synchronous file IO, once
            // per photo — an album is thousands of them. On a stalled NFS mount
            // that would block an async worker thread, so it runs on the
            // blocking pool like the book-archive reads do.
            let owned = path.clone();
            let tags = tokio::task::spawn_blocking(move || read_exif(&owned))
                .await
                .ok()
                .flatten();
            if let Some(tags) = tags {
                apply_exif(item, &tags);
            }
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
/// A flat intermediate so the tag→item mapping (`apply_exif`) stays pure and
/// unit-testable without a file on disk.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExifTags {
    /// The EXIF-only fields.
    pub exif: PhotoExif,
    /// TagLib#'s comment: `IFDTag.Comment` (see `ifd_comment`), and "" for a
    /// file TagLib would open that holds none (see `read_exif`).
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

/// Reads `path`'s EXIF tags, or `None` when TagLib would fail to open it.
///
/// Port of the C# `TagLib.File.Create` + `catch (Exception)` block, and the
/// distinction matters: TagLib opens an image with no EXIF quite happily and
/// hands back an `ImageTag` whose fields are all null, which upstream then
/// assigns — clearing the item's camera/GPS/comment. A file TagLib cannot
/// open throws, and upstream leaves the item alone.
///
/// TagLib picks its reader by the extension, and that reader throws on a file
/// that does not start as its format does, a PNG saved as `.jpg` among them
/// (see `taglib_header`), and on a broken container or EXIF block. Each was
/// measured against Jellyfin 12.4 (gl-1611). A broken IFD it only marks
/// corrupt and reads on; kamadak-exif refuses such a file, which is left
/// alone here (gl-1615). So a container that reads with no EXIF in it yields
/// DEFAULT tags, the clearing case; a mismatched, broken or unreadable file
/// yields `None`, the leave-alone case.
///
/// Ferrofin reads EXIF from containers TagLib reads none from: WebP and AVIF,
/// which it cannot open, and a PNG's `eXIf` chunk, which its PNG reader skips.
/// gl-1614 holds that choice for the owner.
fn read_exif(path: &str) -> Option<ExifTags> {
    let file = std::fs::File::open(path).ok()?;
    // A failed stat skips the IFD walk and leaves the full read to decide. A
    // file cut short after this stat can still read as it was; its new write
    // time has it read again on the next scan.
    let len = file.metadata().map_or(0, |meta| meta.len());
    let mut reader = BufReader::new(Metered::new(file));
    let header = taglib_header(path);
    if let Some(header_matches) = header
        && !header_matches(reader.fill_buf().ok()?)
    {
        return None;
    }
    let mut tags = exif_tags(&mut reader, len)?;
    // TagLib#'s combined `ImageTag.Comment` (`CombinedImageTag.cs:621-631`) is
    // never null: it is "" when no tag holds a comment. Until gl-1614 decides,
    // a WebP or an AVIF, which TagLib cannot open, keeps only a comment its
    // EXIF holds.
    if header.is_some() {
        tags.comment.get_or_insert_default();
    } else {
        tags.comment = tags.comment.filter(|comment| !comment.is_empty());
    }
    Some(tags)
}

/// The header check of the reader TagLibSharp 2.3.0 picks for `path`'s
/// extension (Jpeg and Png `ValidateHeader`, `BaseTiffFile.ReadHeader`, Cr2
/// `ReadAdditionalCR2Header`), or `None` for an extension of
/// [`EXIF_INCLUDE_EXTENSIONS`] it has no reader for: WebP and AVIF, where
/// upstream's `TagLib.File.Create` throws and the photo is left alone.
/// Ferrofin reads those by their content.
fn taglib_header(path: &str) -> Option<fn(&[u8]) -> bool> {
    match extension_of(path).as_str() {
        ".jpg" | ".jpeg" => Some(|head| head.starts_with(b"\xff\xd8")),
        ".png" => Some(|head| head.starts_with(b"\x89PNG\r\n\x1a\n")),
        ".tiff" => Some(tiff_header),
        // After the TIFF header: "CR", version 2.0, and the RAW IFD's offset.
        ".cr2" => Some(|head| {
            tiff_header(head)
                && head
                    .get(8..16)
                    .is_some_and(|cr2| cr2.starts_with(b"CR\x02\x00"))
        }),
        _ => None,
    }
}

/// Whether `head` starts as a TIFF does: a byte order, then the magic 42.
fn tiff_header(head: &[u8]) -> bool {
    head.starts_with(b"II\x2a\x00") || head.starts_with(b"MM\x00\x2a")
}

/// A reader that counts the bytes it returns, so the IFD walk knows what it
/// really read through the buffer in front of it.
struct Metered<R> {
    /// The file.
    inner: R,
    /// The bytes read from it so far.
    read: u64,
}

impl<R> Metered<R> {
    /// `inner`, with nothing read yet.
    fn new(inner: R) -> Self {
        Self { inner, read: 0 }
    }
}

impl<R: Read> Read for Metered<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read = self
            .read
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        Ok(n)
    }

    // Forwarded so the full read keeps the file's own `read_to_end`, which
    // sizes its buffer from the file's length once.
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        let n = self.inner.read_to_end(buf)?;
        self.read = self
            .read
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        Ok(n)
    }
}

impl<R: Seek> Seek for Metered<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// [`read_exif`] over an open file of `len` bytes.
///
/// A TIFF-based file (`.tiff`, `.cr2`) goes through [`primary_tiff`], which
/// reads its IFDs instead of the whole file. Any other container, and any
/// TIFF that walk turns down, goes through kamadak-exif's own reader. `None`
/// is a file TagLib would refuse (see `tags_or_unopened`).
fn exif_tags<R: Read + Seek>(reader: &mut BufReader<Metered<R>>, len: u64) -> Option<ExifTags> {
    // A peek leaves the reader at the start, so a JPEG, PNG or WebP is read
    // exactly as before.
    let is_tiff = reader.fill_buf().is_ok_and(tiff_header);
    let walked = is_tiff
        .then(|| primary_tiff(reader, len))
        .flatten()
        .and_then(|tiff| exif::Reader::new().read_raw(tiff).ok());
    let parsed = match walked {
        Some(exif) => Ok(exif),
        // The walk moved the reader: the full read starts from the top.
        None if is_tiff => reader
            .rewind()
            .map_err(exif::Error::from)
            .and_then(|()| exif::Reader::new().read_from_container(reader)),
        None => exif::Reader::new().read_from_container(reader),
    };
    tags_or_unopened(parsed)
}

/// The tags of a parse, or `None` for a file TagLib would fail to open: any
/// failure but a container that holds no EXIF (see `read_exif`).
fn tags_or_unopened(parsed: Result<exif::Exif, exif::Error>) -> Option<ExifTags> {
    match parsed {
        Ok(exif) => Some(tags_from(&exif)),
        Err(exif::Error::NotFound(_)) => Some(ExifTags::default()),
        Err(_) => None,
    }
}

/// The most IFDs kamadak-exif 0.6.1's parser reads in one chain before it
/// rejects the file (`tiff.rs`, `parse_body`).
const PARSER_IFD_LIMIT: u16 = 8;

/// The TIFF `LONG` field type, the type of a sub-IFD pointer.
const TIFF_LONG: u16 = 4;

/// Copies a TIFF-based file's [`PRIMARY_TAGS`] into a small TIFF of their
/// own, reading only the file's IFDs and those tags' values.
///
/// kamadak-exif loads a whole TIFF into memory before it parses one, and a
/// RAW or scanned TIFF runs to tens or hundreds of megabytes, while its tags
/// sit in a few kilobytes of IFDs, often after the pixel data (libtiff and
/// ffmpeg write IFD0 last). This walks the IFD chain with seeks, making every
/// check kamadak-exif 0.6.1's parser makes against the whole file (0.6.2 only
/// adds a refusal of an IFD reached twice, which this walk declines too), and
/// keeps the listed entries of IFD0 and of its Exif and GPS IFDs, which parse
/// to the same fields.
///
/// `None` hands the file back to the full read: not a TIFF, an I/O error, a
/// check the parser would fail, a shape this walk does not follow (a sub-IFD
/// pointer that is not one `LONG`, an IFD reached twice), or a walk that would
/// cost more than reading the whole file (overlapping or scattered IFDs and
/// values). The full read then decides as it always has.
///
/// `reader` must be at the start of the file.
fn primary_tiff<R: Read + Seek>(reader: &mut BufReader<Metered<R>>, len: u64) -> Option<Vec<u8>> {
    let mut walk = TiffWalk {
        reader,
        position: 0,
        len,
        budget: len,
        little_endian: false,
        seen: HashSet::new(),
        kept: Vec::new(),
    };
    let header = walk.read_at(0, 8)?;
    walk.little_endian = match header.get(..4)? {
        b"II\x2a\x00" => true,
        b"MM\x00\x2a" => false,
        _ => return None,
    };
    let mut next = walk.load_u32(&header[4..]);
    let mut ifd_num = 0;
    while next != 0 {
        if ifd_num == PARSER_IFD_LIMIT {
            return None;
        }
        next = walk.ifd(next, exif::Context::Tiff, ifd_num)?;
        ifd_num += 1;
    }
    walk.compact()
}

/// One kept IFD entry: its tag, field type, count and value bytes (the four
/// inline bytes, or the out-of-line value).
type KeptEntry = (exif::Tag, u16, u32, Vec<u8>);

/// The state of one [`primary_tiff`] walk.
struct TiffWalk<'a, R> {
    /// The file.
    reader: &'a mut BufReader<Metered<R>>,
    /// Where `reader` is, so a read near the last one can stay inside its
    /// buffer.
    position: u64,
    /// The file's length, which every offset is checked against.
    len: u64,
    /// What the walk may still ask for: the file's length, the cost of the
    /// full read it replaces.
    budget: u64,
    /// The file's byte order.
    little_endian: bool,
    /// The offsets of the IFDs walked so far.
    seen: HashSet<u32>,
    /// The [`PRIMARY_TAGS`] entries found, one per tag: the last one, which
    /// is the one the parser returns.
    kept: Vec<KeptEntry>,
}

impl<R: Read + Seek> TiffWalk<'_, R> {
    /// The `size` bytes at `offset`, or `None` when they run past the end of
    /// the file, would take the walk past its budget or cannot be read.
    fn read_at(&mut self, offset: u64, size: u64) -> Option<Vec<u8>> {
        let end = offset.checked_add(size)?;
        if end > self.len {
            return None;
        }
        // Every read is charged its size, which a file's disjoint IFDs and
        // values keep within its length.
        self.budget = self.budget.checked_sub(size)?;
        let step = i64::try_from(offset).ok()? - i64::try_from(self.position).ok()?;
        self.reader.seek_relative(step).ok()?;
        let mut bytes = vec![0; usize::try_from(size).ok()?];
        self.reader.read_exact(&mut bytes).ok()?;
        self.position = end;
        // Neither may what the file gave up, the buffer's refills included.
        if self.reader.get_ref().read > self.len {
            return None;
        }
        Some(bytes)
    }

    /// The `u16` at the start of `bytes`, in the file's byte order.
    fn load_u16(&self, bytes: &[u8]) -> u16 {
        let bytes = [bytes[0], bytes[1]];
        if self.little_endian {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        }
    }

    /// The `u32` at the start of `bytes`, in the file's byte order.
    fn load_u32(&self, bytes: &[u8]) -> u32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if self.little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }
    }

    /// Walks the IFD at `offset`, read in context `ctx` as part of IFD
    /// `ifd_num`, and the sub-IFDs it points to. Returns the offset of the
    /// next IFD in the chain.
    fn ifd(&mut self, offset: u32, ctx: exif::Context, ifd_num: u16) -> Option<u32> {
        if !self.seen.insert(offset) {
            return None;
        }
        let count = self.read_at(offset.into(), 2)?;
        let count = u64::from(self.load_u16(&count));
        let body = self.read_at(u64::from(offset) + 2, count * 12 + 4)?;
        let (entries, next) = body.split_at(body.len() - 4);
        for entry in entries.as_chunks::<12>().0 {
            let tag = exif::Tag(ctx, self.load_u16(entry));
            let kind = self.load_u16(&entry[2..]);
            let count = self.load_u32(&entry[4..]);
            let inline = &entry[8..];
            let child = match tag {
                exif::Tag::ExifIFDPointer => Some(exif::Context::Exif),
                exif::Tag::GPSInfoIFDPointer => Some(exif::Context::Gps),
                exif::Tag::InteropIFDPointer => Some(exif::Context::Interop),
                _ => None,
            };
            if let Some(child) = child {
                // The parser takes any unsigned integer here; this follows
                // the single LONG that writers use.
                if kind != TIFF_LONG || count != 1 {
                    return None;
                }
                let pointer = self.load_u32(inline);
                // A sub-IFD that chains on is an error to the parser.
                if self.ifd(pointer, child, ifd_num)? != 0 {
                    return None;
                }
                continue;
            }
            let keep = ifd_num == 0 && PRIMARY_TAGS.contains(&tag);
            let size = field_unit_len(kind) * u64::from(count);
            let value = if size <= 4 {
                inline.to_vec()
            } else {
                let at = u64::from(self.load_u32(inline));
                // The parser rejects the whole file for any value past its
                // end, read or not.
                if at + size > self.len {
                    return None;
                }
                if !keep {
                    continue;
                }
                self.read_at(at, size)?
            };
            if keep {
                self.kept.retain(|(kept, ..)| *kept != tag);
                self.kept.push((tag, kind, count, value));
            }
        }
        Some(self.load_u32(next))
    }

    /// The kept entries as a TIFF of their own, in the file's byte order:
    /// IFD0 holding the TIFF tags and pointers to an Exif and a GPS IFD
    /// holding theirs, then the out-of-line values.
    fn compact(self) -> Option<Vec<u8>> {
        let little_endian = self.little_endian;
        let (mut ifd0, mut exif, mut gps) = (Vec::new(), Vec::new(), Vec::new());
        for (tag, kind, count, value) in self.kept {
            let rows = match tag.0 {
                exif::Context::Tiff => &mut ifd0,
                exif::Context::Exif => &mut exif,
                exif::Context::Gps => &mut gps,
                // No Interop tag is read; one listed would need its own IFD.
                _ => return None,
            };
            rows.push((tag.1, kind, count, value));
        }

        let ifd_len = |rows: usize| 2 + 12 * rows + 4;
        let pointers = usize::from(!exif.is_empty()) + usize::from(!gps.is_empty());
        let exif_at = 8 + ifd_len(ifd0.len() + pointers);
        let gps_at = exif_at
            + if exif.is_empty() {
                0
            } else {
                ifd_len(exif.len())
            };
        let mut values_at = gps_at
            + if gps.is_empty() {
                0
            } else {
                ifd_len(gps.len())
            };
        let children = [
            (exif::Tag::ExifIFDPointer.1, &exif, exif_at),
            (exif::Tag::GPSInfoIFDPointer.1, &gps, gps_at),
        ];
        for (pointer, rows, at) in children {
            if !rows.is_empty() {
                let at = u32_bytes(little_endian, u32::try_from(at).ok()?);
                ifd0.push((pointer, TIFF_LONG, 1, at.to_vec()));
            }
        }

        let mut tiff = Vec::from(if little_endian {
            b"II\x2a\x00"
        } else {
            b"MM\x00\x2a"
        });
        tiff.extend_from_slice(&u32_bytes(little_endian, 8));
        let children = [exif, gps].into_iter().filter(|rows| !rows.is_empty());
        let directories: Vec<_> = std::iter::once(ifd0).chain(children).collect();
        for rows in &directories {
            tiff.extend_from_slice(&u16_bytes(little_endian, u16::try_from(rows.len()).ok()?));
            for (tag, kind, count, value) in rows {
                tiff.extend_from_slice(&u16_bytes(little_endian, *tag));
                tiff.extend_from_slice(&u16_bytes(little_endian, *kind));
                tiff.extend_from_slice(&u32_bytes(little_endian, *count));
                if value.len() <= 4 {
                    tiff.extend_from_slice(value);
                } else {
                    tiff.extend_from_slice(&u32_bytes(
                        little_endian,
                        u32::try_from(values_at).ok()?,
                    ));
                    values_at += value.len();
                }
            }
            tiff.extend_from_slice(&u32_bytes(little_endian, 0));
        }
        // The out-of-line values, in the order their offsets were handed out.
        for (.., value) in directories.iter().flatten() {
            if value.len() > 4 {
                tiff.extend_from_slice(value);
            }
        }
        Some(tiff)
    }
}

/// `value` in the byte order `little_endian` picks.
fn u16_bytes(little_endian: bool, value: u16) -> [u8; 2] {
    if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    }
}

/// `value` in the byte order `little_endian` picks.
fn u32_bytes(little_endian: bool, value: u32) -> [u8; 4] {
    if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    }
}

/// The size of one value of TIFF field type `kind` as kamadak-exif 0.6.1's
/// parser sizes it (`value.rs`, `get_type_info`): a type it does not know takes none.
fn field_unit_len(kind: u16) -> u64 {
    match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

/// Every primary-image tag [`tags_from`] reads. [`primary_tiff`] keeps only
/// these, so a TIFF-based file's other values are never loaded.
const PRIMARY_TAGS: [exif::Tag; 23] = [
    exif::Tag::Make,
    exif::Tag::Model,
    exif::Tag::Software,
    exif::Tag::Orientation,
    exif::Tag::ImageDescription,
    exif::Tag::DateTime,
    XP_TITLE,
    RATING,
    exif::Tag::ApertureValue,
    exif::Tag::ShutterSpeedValue,
    exif::Tag::ExposureTime,
    exif::Tag::FocalLength,
    exif::Tag::PhotographicSensitivity,
    exif::Tag::UserComment,
    exif::Tag::DateTimeOriginal,
    exif::Tag::PixelXDimension,
    exif::Tag::PixelYDimension,
    exif::Tag::GPSLatitude,
    exif::Tag::GPSLatitudeRef,
    exif::Tag::GPSLongitude,
    exif::Tag::GPSLongitudeRef,
    exif::Tag::GPSAltitude,
    exif::Tag::GPSAltitudeRef,
];

/// The primary image's `tag`. Every lookup [`tags_from`] makes goes through
/// here, so a tag read without being listed in [`PRIMARY_TAGS`] (and so
/// missing from every TIFF-based file) fails the tests.
fn primary(exif: &exif::Exif, tag: exif::Tag) -> Option<&exif::Field> {
    debug_assert!(
        PRIMARY_TAGS.contains(&tag),
        "{tag} is read but missing from PRIMARY_TAGS"
    );
    exif.get_field(tag, exif::In::PRIMARY)
}

/// Projects an [`exif::Exif`] onto [`ExifTags`]. Pure — the seam the mapping
/// tests drive with a synthetic in-memory image.
fn tags_from(exif: &exif::Exif) -> ExifTags {
    use exif::Tag;

    let rational = |tag: Tag| -> Option<f64> {
        match primary(exif, tag)?.value {
            exif::Value::Rational(ref v) => v.first().map(exif::Rational::to_f64),
            exif::Value::SRational(ref v) => v.first().map(exif::SRational::to_f64),
            _ => None,
        }
    };
    // Read the raw bytes, never `display_value()`: that renders an ASCII field
    // quoted with every non-ASCII byte escaped (`café` → `caf\xc3\xa9`), and
    // renders an unknown BYTE field as a comma-separated list of numbers.
    let text = |tag: Tag| -> Option<String> {
        let field = primary(exif, tag)?;
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
        primary(exif, tag)?
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
        comment: ifd_comment(exif),
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

/// TagLib#'s `IFDTag.Comment` (TagLibSharp 2.3.0, `IFDTag.cs:162-171`): the
/// Exif IFD's `UserComment` whenever TagLib reads that entry as one, even an
/// empty one, and only otherwise the `ImageDescription` up to its first NUL.
/// Neither is trimmed.
///
/// TagLib's reader (`IFDReader.cs:382-526`) makes a `UserCommentIFDEntry` only
/// of an entry with a count over 4 that its type branches do not take first:
/// UNDEFINED, SBYTE, FLOAT, DOUBLE or an unknown type. Any other `UserComment`
/// is no comment, and the description shows through.
fn ifd_comment(exif: &exif::Exif) -> Option<String> {
    // shortcut: of the types TagLib takes, only UNDEFINED is read here; no
    // writer is known to use SBYTE, FLOAT, DOUBLE or an unknown type for one.
    if let Some(exif::Value::Undefined(bytes, _)) =
        primary(exif, exif::Tag::UserComment).map(|field| &field.value)
        && bytes.len() > 4
    {
        return Some(user_comment(bytes));
    }
    // kamadak-exif splits an ASCII value at each NUL.
    match &primary(exif, exif::Tag::ImageDescription)?.value {
        exif::Value::Ascii(parts) => parts
            .first()
            .map(|text| String::from_utf8_lossy(text).into_owned()),
        _ => None,
    }
}

/// An EXIF `UserComment`'s text, decoded as TagLib#'s `UserCommentIFDEntry`
/// does (`UserCommentIFDEntry.cs:108-168`), branch for branch.
///
/// The character sets are the one difference: TagLib reads `ASCII` text as
/// Latin-1 and `UNICODE` text as UTF-8, so a non-ASCII comment comes out
/// garbled (`café` as `cafÃ©`, UTF-16 cut at its first zero byte). This reads
/// UTF-8 and UTF-16, as the EXIF specification means.
fn user_comment(bytes: &[u8]) -> String {
    let up_to_nul = |text: &[u8]| {
        let text = text.split(|byte| *byte == 0).next().unwrap_or_default();
        String::from_utf8_lossy(text).into_owned()
    };
    if let Some(text) = bytes.strip_prefix(b"ASCII\0\0\0") {
        return up_to_nul(text);
    }
    if let Some(text) = bytes.strip_prefix(b"UNICODE\0") {
        return utf16_with_bom(text);
    }
    let whole = String::from_utf8_lossy(bytes);
    let trimmed = whole.trim();
    if trimmed.is_empty() || trimmed == "\0" {
        return String::new();
    }
    // The all-zero "undefined" code, and CanonZoomBrowser's zero byte followed
    // by seven bytes of junk.
    if let [0, _, _, _, _, _, _, _, text @ ..] = bytes {
        return up_to_nul(text);
    }
    // Any other code (JIS among them) is read as text, the code included,
    // except a corrupt `Unicode` one.
    up_to_nul(bytes.strip_prefix(b"Unicode\0").unwrap_or(bytes))
}

/// Decodes UTF-16 text, honouring a leading byte-order mark and defaulting to
/// big-endian as the EXIF spec specifies.
fn utf16_with_bom(bytes: &[u8]) -> String {
    let (bytes, little_endian) = match bytes {
        [0xff, 0xfe, rest @ ..] => (rest, true),
        // A big-endian BOM and no BOM at all decode the same way: EXIF
        // specifies big-endian as the default.
        [0xfe, 0xff, rest @ ..] | rest => (rest, false),
    };
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Decodes a UTF-16LE byte run, as the Windows `XP*` TIFF tags store text.
fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
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
    let field = primary(exif, tag)?;
    let exif::Value::Rational(ref dms) = field.value else {
        return None;
    };
    let [d, m, sec] = dms.get(..3)? else {
        return None;
    };
    let degrees = d.to_f64() + m.to_f64() / 60.0 + sec.to_f64() / 3600.0;
    let hemisphere = primary(exif, ref_tag).map(|f| f.display_value().to_string());
    let negative = hemisphere
        .as_deref()
        .is_some_and(|h| h.trim().starts_with(negative_ref));
    Some(if negative { -degrees } else { degrees })
}

/// GPS altitude in metres; `GPSAltitudeRef == 1` means below sea level.
fn gps_altitude(exif: &exif::Exif) -> Option<f64> {
    let field = primary(exif, exif::Tag::GPSAltitude)?;
    let exif::Value::Rational(ref v) = field.value else {
        return None;
    };
    let metres = v.first()?.to_f64();
    let below =
        primary(exif, exif::Tag::GPSAltitudeRef).and_then(|f| f.value.get_uint(0)) == Some(1);
    Some(if below { -metres } else { metres })
}

/// The date the photo was taken: `DateTimeOriginal`, else `DateTime`. EXIF
/// stores local time with no zone, which C# also treats as local — Ferrofin
/// stamps it as UTC so the value round-trips unchanged.
fn exif_datetime(exif: &exif::Exif) -> Option<DateTime<Utc>> {
    for tag in [exif::Tag::DateTimeOriginal, exif::Tag::DateTime] {
        let Some(field) = primary(exif, tag) else {
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
/// the tags carry positive ones. Unconditional means a photo whose rating or
/// comment tag was REMOVED has the field cleared on the next refresh — the file
/// is the only source of truth for a photo, and upstream assigns both straight
/// from the tag. It runs for every file TagLib would open, one with no EXIF
/// included; a file it would fail to open never reaches here and keeps
/// whatever it had.
fn apply_exif(item: &mut PhotoItem, tags: &ExifTags) {
    item.exif_was_read = true;
    item.exif = tags.exif.clone();
    if let Some(width) = tags.width.filter(|w| *w > 0) {
        item.width = width;
    }
    if let Some(height) = tags.height.filter(|h| *h > 0) {
        item.height = height;
    }
    item.community_rating = tags.rating;
    item.overview.clone_from(&tags.comment);
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

#[cfg(test)]
mod tests {
    //! Round-trip cases transliterated from `PhotoProvider.cs`: a zero-width
    //! photo is backfilled from the processor; a photo with positive stored
    //! dimensions is left alone; the primary image path is always set; the EXIF
    //! extension gate accepts the seven photo types and excludes everything
    //! else; and the reported update type is `ImageUpdate`.
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
            _item_path: Option<&str>,
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
    fn name_is_embedded_information() {
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(1, 1)));
        assert_eq!(provider.name(), "Embedded Information");
    }

    /// A file on disk whose last write time is `modified`.
    fn file_modified_at(dir: &tempfile::TempDir, modified: DateTime<Utc>) -> String {
        // `.gif` stays out of the EXIF branch: these tests are about the image row.
        let path = write_temp(dir, "photo.gif", b"GIF89a");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(modified.into())
            .expect("set mtime");
        path
    }

    #[tokio::test]
    async fn fetch_dates_a_new_primary_image_by_the_files_last_write_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modified = ts(1_700_000_000);
        let path = file_modified_at(&dir, modified);
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(10, 20)));
        let mut item = PhotoItem {
            path: path.clone(),
            ..Default::default()
        };

        provider.fetch(&mut item).await.expect("fetch");

        let image = item.image_info(ImageType::Primary).expect("primary");
        assert_eq!(image.path, path);
        assert_eq!(image.date_modified, modified);
        assert_eq!((image.width, image.height), (0, 0));
    }

    /// `BaseItem.SetImagePath`: "reset these values". The blurhash is not one
    /// of them: it stands until the image pass measures the file.
    #[tokio::test]
    async fn fetch_resets_the_size_of_an_existing_primary_image() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modified = ts(1_700_000_000);
        let path = file_modified_at(&dir, modified);
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(10, 20)));
        let mut item = PhotoItem {
            path: path.clone(),
            images: vec![ItemImageInfo {
                path: "/moved/from/here.gif".to_owned(),
                image_type: ImageType::Primary,
                date_modified: ts(1_000),
                width: 6000,
                height: 4000,
                blur_hash: Some("stale".to_owned()),
            }],
            ..Default::default()
        };

        provider.fetch(&mut item).await.expect("fetch");

        let [image] = item.images.as_slice() else {
            panic!(
                "the existing row is updated, not added to: {:?}",
                item.images
            );
        };
        assert_eq!(image.path, path);
        assert_eq!(image.date_modified, modified);
        assert_eq!((image.width, image.height), (0, 0));
        assert_eq!(image.blur_hash.as_deref(), Some("stale"));
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
        // SOI and nothing else: TagLib throws, so the photo is left alone.
        let path = write_temp(&dir, "empty.jpg", b"\xff\xd8");
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(4, 3)));
        let mut item = PhotoItem {
            path,
            ..Default::default()
        };
        provider.fetch(&mut item).await.expect("fetch");
        assert!(!item.exif_was_read);
        assert_eq!(item.exif, PhotoExif::default());
        // The always-on dimension backfill still runs.
        assert_eq!((item.width, item.height), (4, 3));
    }

    #[tokio::test]
    async fn a_file_reaching_one_ifd_twice_is_left_alone() {
        // IFD0 points twice at one Exif IFD holding ISO 400. kamadak-exif
        // 0.6.1 read that IFD again for every pointer, which nested pointers
        // multiply (GHSA-53gf-vqf6-qvp8); 0.6.2 refuses the file.
        const EXIF_IFD_TWICE: &[u8] = b"MM\x00\x2a\x00\x00\x00\x08\
            \x00\x02\x87\x69\x00\x04\x00\x00\x00\x01\x00\x00\x00\x26\
            \x87\x69\x00\x04\x00\x00\x00\x01\x00\x00\x00\x26\x00\x00\x00\x00\
            \x00\x01\x88\x27\x00\x03\x00\x00\x00\x01\x01\x90\x00\x00\x00\x00\x00\x00";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_temp(&dir, "twice.tiff", EXIF_IFD_TWICE);
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(1, 1)));
        let mut item = PhotoItem {
            path,
            width: 1,
            height: 1,
            ..Default::default()
        };
        provider.fetch(&mut item).await.expect("fetch");
        assert_eq!(item.exif, PhotoExif::default());
        // kamadak-exif refuses the file, which leaves the photo alone as a
        // broken EXIF block does. TagLib marks such a loop corrupt and reads
        // on (gl-1615).
        assert!(!item.exif_was_read);
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

    /// One IFD row for [`tiff_file`]: tag, type, count and the value's bytes,
    /// placed out of line when over four bytes, as TIFF does.
    type Row = (u16, u16, u32, Vec<u8>);

    /// A TIFF laid out the way libtiff and ffmpeg write one: the header, then
    /// `image` bytes of pixel data, then the values and IFDs, so IFD0 comes
    /// last. IFD0 holds `ifd0` plus pointers to an Exif IFD (`exif`, which
    /// points on to an Interop IFD) and a GPS IFD (`gps`); IFD1 holds `ifd1`.
    fn tiff_file(
        little_endian: bool,
        image: usize,
        ifd0: &[Row],
        exif: &[Row],
        gps: &[Row],
        ifd1: &[Row],
    ) -> Vec<u8> {
        let pointer = |tag: u16, at: u32| (tag, 4, 1, u32_bytes(little_endian, at).to_vec());
        let mut file = Vec::from(if little_endian {
            b"II\x2a\x00"
        } else {
            b"MM\x00\x2a"
        });
        file.extend_from_slice(&[0; 4]); // IFD0's offset, patched below
        file.resize(file.len() + image, 0x5a);

        let interop = append_ifd(
            &mut file,
            little_endian,
            &[(0x0001, 2, 4, b"R98\0".to_vec())],
            0,
        );
        let exif = [exif, &[pointer(0xa005, interop)]].concat();
        let exif = append_ifd(&mut file, little_endian, &exif, 0);
        let gps = append_ifd(&mut file, little_endian, gps, 0);
        let ifd1 = append_ifd(&mut file, little_endian, ifd1, 0);
        let ifd0 = [ifd0, &[pointer(0x8769, exif), pointer(0x8825, gps)]].concat();
        let ifd0 = append_ifd(&mut file, little_endian, &ifd0, ifd1);
        file[4..8].copy_from_slice(&u32_bytes(little_endian, ifd0));
        file
    }

    /// Appends `rows`' out-of-line values and then their IFD, chained to
    /// `next`, and returns the IFD's offset.
    fn append_ifd(file: &mut Vec<u8>, little_endian: bool, rows: &[Row], next: u32) -> u32 {
        let mut fields = Vec::new();
        for (tag, kind, count, value) in rows {
            let inline = if value.len() <= 4 {
                let mut inline = value.clone();
                inline.resize(4, 0);
                inline
            } else {
                let at = u32::try_from(file.len()).unwrap();
                file.extend_from_slice(value);
                u32_bytes(little_endian, at).to_vec()
            };
            fields.extend_from_slice(&u16_bytes(little_endian, *tag));
            fields.extend_from_slice(&u16_bytes(little_endian, *kind));
            fields.extend_from_slice(&u32_bytes(little_endian, *count));
            fields.extend_from_slice(&inline);
        }
        let at = u32::try_from(file.len()).unwrap();
        file.extend_from_slice(&u16_bytes(
            little_endian,
            u16::try_from(rows.len()).unwrap(),
        ));
        file.extend_from_slice(&fields);
        file.extend_from_slice(&u32_bytes(little_endian, next));
        at
    }

    /// RATIONAL (or SRATIONAL) values in the file's byte order.
    fn rationals(little_endian: bool, values: &[(u32, u32)]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|&(n, d)| [u32_bytes(little_endian, n), u32_bytes(little_endian, d)])
            .flatten()
            .collect()
    }

    /// A camera file carrying every tag [`tags_from`] reads, beside the bulky
    /// ones it does not (an ICC profile, Photoshop's layers, a maker note,
    /// strip offsets) and an IFD1 thumbnail with its own make, after `image`
    /// bytes of pixel data.
    fn camera_tiff(little_endian: bool, image: usize) -> Vec<u8> {
        let short = |v: u16| u16_bytes(little_endian, v).to_vec();
        let long = |v: u32| u32_bytes(little_endian, v).to_vec();
        let xp_title: Vec<u8> = "Harbour"
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        let ifd0: Vec<Row> = vec![
            (0x010f, 2, 6, b"Wrong\0".to_vec()), // a duplicate: the last Make wins
            (0x010f, 2, 6, b"Canon\0".to_vec()),
            (0x0110, 2, 13, b"Canon EOS R5\0".to_vec()),
            (0x0111, 4, 64, vec![0x11; 256]), // StripOffsets, not read
            (0x0112, 3, 1, short(6)),
            (0x010e, 2, 14, b"A description\0".to_vec()),
            (0x0131, 2, 15, b"Firmware 1.8.1\0".to_vec()),
            (0x0132, 2, 20, b"2024:05:06 07:08:09\0".to_vec()),
            (0x4746, 3, 1, short(4)),
            (0x8773, 7, 200_000, vec![0x22; 200_000]), // ICC profile, not read
            (0x935c, 7, 2 << 20, vec![0x44; 2 << 20]), // Photoshop layers, not read
            (0x9c9b, 1, u32::try_from(xp_title.len()).unwrap(), xp_title),
        ];
        let exif: Vec<Row> = vec![
            (0x829a, 5, 1, rationals(little_endian, &[(1, 250)])),
            (0x8827, 3, 1, short(100)), // a duplicate: the last ISO wins
            (0x8827, 3, 1, short(400)),
            (0x9003, 2, 20, b"2023:01:02 03:04:05\0".to_vec()),
            (0x9201, 10, 1, rationals(little_endian, &[(8, 1)])),
            (0x9202, 5, 1, rationals(little_endian, &[(4, 1)])),
            (0x920a, 5, 1, rationals(little_endian, &[(50, 1)])),
            (0x927c, 7, 100_000, vec![0x33; 100_000]), // MakerNote, not read
            (0x9286, 7, 13, b"ASCII\0\0\0Hello".to_vec()),
            (0xa002, 4, 1, long(8192)),
            (0xa003, 4, 1, long(5464)),
        ];
        let gps: Vec<Row> = vec![
            (0x0001, 2, 2, b"S\0".to_vec()),
            (
                0x0002,
                5,
                3,
                rationals(little_endian, &[(33, 1), (52, 1), (12, 1)]),
            ),
            (0x0003, 2, 2, b"W\0".to_vec()),
            (
                0x0004,
                5,
                3,
                rationals(little_endian, &[(151, 1), (12, 1), (36, 1)]),
            ),
            (0x0005, 1, 1, vec![1]),
            (0x0006, 5, 1, rationals(little_endian, &[(12, 1)])),
        ];
        let ifd1: Vec<Row> = vec![(0x010f, 2, 6, b"Thumb\0".to_vec()), (0x0201, 4, 1, long(8))];
        tiff_file(little_endian, image, &ifd0, &exif, &gps, &ifd1)
    }

    /// The tags kamadak-exif's own reader finds when it loads the whole file:
    /// the behaviour every streamed read must reproduce.
    fn whole_file_tags(file: &[u8]) -> Option<ExifTags> {
        tags_or_unopened(exif::Reader::new().read_from_container(&mut std::io::Cursor::new(file)))
    }

    /// [`exif_tags`] over `file`, with the number of bytes it read.
    fn streamed_tags(file: &[u8]) -> (Option<ExifTags>, usize) {
        let mut reader = BufReader::new(Metered::new(std::io::Cursor::new(file)));
        let tags = exif_tags(&mut reader, u64::try_from(file.len()).unwrap());
        (tags, usize::try_from(reader.get_ref().read).unwrap())
    }

    #[test]
    fn a_tiff_is_read_through_its_ifds_not_loaded_whole() {
        // gl-1276: a 10 MiB TIFF with IFD0 after the pixel data, as libtiff
        // writes it, read for a few kilobytes of tags.
        let file = camera_tiff(true, 8 << 20);
        let (tags, read) = streamed_tags(&file);

        assert_eq!(tags, whole_file_tags(&file));
        let tags = tags.expect("a readable TIFF");
        assert_eq!(tags.exif.camera_make.as_deref(), Some("Canon"));
        assert_eq!(tags.exif.camera_model.as_deref(), Some("Canon EOS R5"));
        assert_eq!(tags.exif.exposure_time, Some(1.0 / 250.0));
        assert_eq!(tags.exif.iso_speed_rating, Some(400));
        assert_eq!(tags.exif.altitude, Some(-12.0));
        assert!(tags.exif.latitude.is_some_and(|lat| lat < 0.0));
        assert_eq!(tags.title.as_deref(), Some("Harbour"));
        assert_eq!(tags.comment.as_deref(), Some("Hello"));
        assert_eq!(tags.rating, Some(4.0));
        assert_eq!((tags.width, tags.height), (Some(8192), Some(5464)));
        // The IFDs and the values read (in 8 KiB buffer fills, about 64 KiB),
        // not the pixel data, the 2 MiB of layers, the ICC profile or the
        // maker note.
        assert!(read < 128 << 10, "read {read} of {} bytes", file.len());
    }

    /// A big-endian TIFF whose Exif pointer is a SHORT, which kamadak-exif
    /// follows like a LONG: header, IFD0 at 8, the Exif IFD at 26 holding
    /// ExposureTime 1/250 at 44.
    const SHORT_EXIF_POINTER: &[u8] = b"MM\x00\x2a\x00\x00\x00\x08\
        \x00\x01\x87\x69\x00\x03\x00\x00\x00\x01\x00\x1a\x00\x00\x00\x00\x00\x00\
        \x00\x01\x82\x9a\x00\x05\x00\x00\x00\x01\x00\x00\x00\x2c\x00\x00\x00\x00\
        \x00\x00\x00\x01\x00\x00\x00\xfa";

    /// IFD0 holds an inline Make and an ICC profile whose 1000 bytes run past
    /// the end of the file, so kamadak-exif rejects the whole file.
    const TRUNCATED_UNREAD_VALUE: &[u8] = b"MM\x00\x2a\x00\x00\x00\x08\
        \x00\x02\x01\x0f\x00\x02\x00\x00\x00\x03AB\x00\x00\
        \x87\x73\x00\x07\x00\x00\x03\xe8\x00\x00\x01\x00\x00\x00\x00\x00";

    /// IFD0 points twice at one Exif IFD holding ISO 400 (kamadak-exif 0.6.2's
    /// `already_visited_ifd` case, which 0.6.1 reads).
    const EXIF_IFD_TWICE: &[u8] = b"MM\x00\x2a\x00\x00\x00\x08\
        \x00\x02\x87\x69\x00\x04\x00\x00\x00\x01\x00\x00\x00\x26\
        \x87\x69\x00\x04\x00\x00\x00\x01\x00\x00\x00\x26\x00\x00\x00\x00\
        \x00\x01\x88\x27\x00\x03\x00\x00\x00\x01\x01\x90\x00\x00\x00\x00\x00\x00";

    /// IFD0 holds Make "AB" and points at an Exif IFD that chains on to
    /// another IFD, which kamadak-exif rejects.
    const EXIF_IFD_CHAINS_ON: &[u8] = b"MM\x00\x2a\x00\x00\x00\x08\
        \x00\x02\x01\x0f\x00\x02\x00\x00\x00\x03AB\x00\x00\
        \x87\x69\x00\x04\x00\x00\x00\x01\x00\x00\x00\x26\x00\x00\x00\x00\
        \x00\x01\x88\x27\x00\x03\x00\x00\x00\x01\x01\x90\x00\x00\x00\x00\x00\x08";

    /// IFD0 points at two Exif IFDs, holding ISO 100 and then ISO 400.
    const TWO_EXIF_IFDS: &[u8] = b"MM\x00\x2a\x00\x00\x00\x08\
        \x00\x02\x87\x69\x00\x04\x00\x00\x00\x01\x00\x00\x00\x26\
        \x87\x69\x00\x04\x00\x00\x00\x01\x00\x00\x00\x38\x00\x00\x00\x00\
        \x00\x01\x88\x27\x00\x03\x00\x00\x00\x01\x00\x64\x00\x00\x00\x00\x00\x00\
        \x00\x01\x88\x27\x00\x03\x00\x00\x00\x01\x01\x90\x00\x00\x00\x00\x00\x00";

    /// IFD0's Exif pointer is two LONGs, out of line at 26, which kamadak-exif
    /// follows by the first: the Exif IFD at 34 holding ISO 400.
    const EXIF_POINTER_COUNT_TWO: &[u8] = b"II\x2a\x00\x08\x00\x00\x00\
        \x01\x00\x69\x87\x04\x00\x02\x00\x00\x00\x1a\x00\x00\x00\x00\x00\x00\x00\
        \x22\x00\x00\x00\x00\x00\x00\x00\
        \x01\x00\x27\x88\x03\x00\x01\x00\x00\x00\x90\x01\x00\x00\x00\x00\x00\x00";

    /// IFD0's Exif IFD holds ISO 400; IFD1 (a thumbnail) has an Exif IFD of
    /// its own holding ISO 100, which is not the primary image's.
    const EXIF_IFD_UNDER_IFD1: &[u8] = b"II\x2a\x00\x08\x00\x00\x00\
        \x01\x00\x69\x87\x04\x00\x01\x00\x00\x00\x1a\x00\x00\x00\x2c\x00\x00\x00\
        \x01\x00\x27\x88\x03\x00\x01\x00\x00\x00\x90\x01\x00\x00\x00\x00\x00\x00\
        \x01\x00\x69\x87\x04\x00\x01\x00\x00\x00\x3e\x00\x00\x00\x00\x00\x00\x00\
        \x01\x00\x27\x88\x03\x00\x01\x00\x00\x00\x64\x00\x00\x00\x00\x00\x00\x00";

    /// A TIFF whose `ifd1` or `gps` rows (as [`tiff_file`] takes them) hold a
    /// value claiming 100 000 bytes, past the end of the file, beside a
    /// readable Make.
    fn value_past_the_end_in(ifd1: bool) -> Vec<u8> {
        let make = [(0x010f, 2, 3, b"AB\0".to_vec())];
        let past_the_end = [(0x8773, 7, 100_000, vec![0; 8])];
        let (ifd1, gps): (&[Row], &[Row]) = if ifd1 {
            (&past_the_end, &[])
        } else {
            (&[], &past_the_end)
        };
        tiff_file(true, 0, &make, &[], gps, ifd1)
    }

    /// The little-endian start of a TIFF whose IFD0 holds Make "AB" inline,
    /// up to IFD0's next-IFD offset.
    const MAKE_AB: &[u8] =
        b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x0f\x01\x02\x00\x03\x00\x00\x00AB\x00\x00";

    /// Nine chained IFDs, one more than kamadak-exif reads, IFD0 holding
    /// Make "AB".
    fn nine_ifds() -> Vec<u8> {
        let mut file = MAKE_AB.to_vec();
        for _ in 1..9 {
            // The next IFD starts straight after this offset: no entries.
            let next = u32::try_from(file.len() + 4).unwrap();
            file.extend_from_slice(&next.to_le_bytes());
            file.extend_from_slice(&[0, 0]);
        }
        file.extend_from_slice(&0u32.to_le_bytes());
        file
    }

    #[rstest::rstest]
    #[case::little_endian_camera_file(camera_tiff(true, 4096))]
    #[case::big_endian_camera_file(camera_tiff(false, 4096))]
    #[case::exif_pointer_typed_short(SHORT_EXIF_POINTER.to_vec())]
    #[case::unread_value_past_the_end(TRUNCATED_UNREAD_VALUE.to_vec())]
    #[case::exif_ifd_reached_twice(EXIF_IFD_TWICE.to_vec())]
    // IFD0 chains to itself, past the parser's limit of 8 IFDs.
    #[case::ifd_chain_loops(b"MM\x00\x2a\x00\x00\x00\x08\x00\x00\x00\x00\x00\x08".to_vec())]
    #[case::exif_ifd_past_the_end(
        b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x69\x87\x04\x00\x01\x00\x00\x00\xff\x00\x00\x00\x00\x00\x00\x00"
            .to_vec()
    )]
    #[case::exif_ifd_chains_on(EXIF_IFD_CHAINS_ON.to_vec())]
    #[case::nine_ifds(nine_ifds())]
    #[case::two_exif_ifds(TWO_EXIF_IFDS.to_vec())]
    #[case::exif_ifd_under_ifd1(EXIF_IFD_UNDER_IFD1.to_vec())]
    #[case::exif_pointer_count_two(EXIF_POINTER_COUNT_TWO.to_vec())]
    #[case::value_past_the_end_in_ifd1(value_past_the_end_in(true))]
    #[case::value_past_the_end_in_the_gps_ifd(value_past_the_end_in(false))]
    // Make of a type the parser does not know, Model with no characters.
    #[case::read_tags_of_odd_types(tiff_file(
        true,
        0,
        &[
            (0x010f, 13, 5, vec![1, 2, 3, 4]),
            (0x0110, 2, 0, Vec::new()),
            (0x0131, 2, 3, b"V1\0".to_vec()),
        ],
        &[],
        &[],
        &[],
    ))]
    #[case::no_ifd(b"II\x2a\x00\x00\x00\x00\x00".to_vec())]
    #[case::header_only(b"II\x2a\x00".to_vec())]
    #[case::jpeg(jpeg_with_exif(&[(0x0110, 2, 3, *b"X1\0\0")], b"", 26))]
    fn a_streamed_read_finds_what_the_whole_file_read_finds(#[case] file: Vec<u8>) {
        assert_eq!(streamed_tags(&file).0, whole_file_tags(&file));
    }

    #[test]
    fn every_field_type_is_sized_as_the_parser_sizes_it() {
        // Beside Make "AB", an unread tag of each type whose value, if it does
        // not fit inline, would run past the end of the file: the parser
        // rejects the file exactly when the type and count make the value
        // longer than four bytes. Counts 1, 2, 3 and 5 tell every size apart.
        for kind in 0..=13u16 {
            for count in [1u32, 2, 3, 5] {
                let mut file = MAKE_AB.to_vec();
                file[8] = 2; // two entries
                file.extend_from_slice(&0x9999u16.to_le_bytes());
                file.extend_from_slice(&kind.to_le_bytes());
                file.extend_from_slice(&count.to_le_bytes());
                file.extend_from_slice(&0xffffu32.to_le_bytes());
                file.extend_from_slice(&0u32.to_le_bytes());
                assert_eq!(
                    streamed_tags(&file).0,
                    whole_file_tags(&file),
                    "type {kind}, count {count}"
                );
            }
        }
    }

    #[tokio::test]
    async fn fetch_reads_a_tiff_whose_ifds_follow_the_pixel_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_temp(&dir, "photo.TIFF", &camera_tiff(true, 1 << 20));
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(0, 0)));
        let mut item = PhotoItem {
            path,
            ..Default::default()
        };

        provider.fetch(&mut item).await.expect("fetch");

        assert!(item.exif_was_read);
        assert_eq!(item.exif.camera_make.as_deref(), Some("Canon"));
        assert_eq!(item.exif.orientation, Some(ImageOrientation::RightTop));
        assert_eq!((item.width, item.height), (8192, 5464));
        assert_eq!(item.name.as_deref(), Some("Harbour"));
        assert_eq!(item.production_year, Some(2023));
    }

    /// A little-endian TIFF whose IFD0 holds `makes` ASCII Make entries of
    /// `size` bytes, the n-th pointing at `at(n)` in the `area` bytes after
    /// the IFD (`at` counts from the area's start).
    fn many_makes(makes: u16, size: u32, area: usize, at: impl Fn(u16) -> u32) -> Vec<u8> {
        let area_at = 8 + 2 + 12 * u32::from(makes) + 4;
        let mut file = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
        file.extend_from_slice(&makes.to_le_bytes());
        for n in 0..makes {
            file.extend_from_slice(&0x010fu16.to_le_bytes());
            file.extend_from_slice(&2u16.to_le_bytes());
            file.extend_from_slice(&size.to_le_bytes());
            file.extend_from_slice(&(area_at + at(n)).to_le_bytes());
        }
        file.extend_from_slice(&0u32.to_le_bytes());
        file.resize(file.len() + area, b'A');
        file
    }

    #[rstest::rstest]
    // 2000 Makes all claiming one 100 000-byte value: reading each would read
    // 200 MB of a 124 KB file.
    #[case::one_bulky_value_many_times(many_makes(2000, 100_000, 100_000, |_| 0))]
    // 20 000 five-byte Makes alternating between two values 64 KiB apart:
    // each read refills the reader's 8 KiB buffer, 160 MB in all.
    #[case::small_values_far_apart(many_makes(20_000, 5, 70_000, |n| (u32::from(n) % 2) << 16))]
    fn a_walk_never_reads_more_than_the_file(#[case] file: Vec<u8>) {
        let (tags, read) = streamed_tags(&file);

        assert_eq!(tags, whole_file_tags(&file));
        // Past the file's length the walk hands over to the full read: at most
        // both, with buffer slack.
        assert!(read < 3 * file.len(), "read {read} of {} bytes", file.len());
    }

    #[test]
    fn a_failed_stat_falls_back_to_the_full_read() {
        let file = camera_tiff(true, 4096);
        let tags = exif_tags(
            &mut BufReader::new(Metered::new(std::io::Cursor::new(&file))),
            0,
        );
        assert_eq!(tags, whole_file_tags(&file));
        assert_eq!(
            tags.and_then(|tags| tags.exif.camera_make).as_deref(),
            Some("Canon")
        );
    }

    /// A reader whose reads fail from `fail_at` on, as a dying disk or a
    /// dropped mount does.
    struct FailingReader {
        inner: std::io::Cursor<Vec<u8>>,
        fail_at: u64,
    }

    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = self.fail_at.saturating_sub(self.inner.position());
            if left == 0 {
                return Err(std::io::Error::other("injected read error"));
            }
            let len = buf.len().min(usize::try_from(left).unwrap());
            self.inner.read(&mut buf[..len])
        }
    }

    impl Seek for FailingReader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn a_read_error_during_the_walk_reads_as_the_full_read_does() {
        // IFD0 sits after the pixel data, past the point reads start failing:
        // the walk gives up and the full read fails as it always did, which
        // leaves the photo alone, as TagLib's throw does upstream.
        let file = camera_tiff(true, 64 << 10);
        let len = u64::try_from(file.len()).unwrap();
        let mut reader = BufReader::new(Metered::new(FailingReader {
            inner: std::io::Cursor::new(file),
            fail_at: 32 << 10,
        }));
        assert_eq!(exif_tags(&mut reader, len), None);
    }

    /// A one-pixel JPEG holding `ifd0`'s entries and, when `exif` has any, an
    /// Exif IFD with them; values over four bytes go out of line. No APP1 when
    /// both are empty. It has the frame header every image has, which TagLib
    /// needs for the dimensions upstream reads before the comment.
    fn jpeg_with_tags(ifd0: &[(u16, u16, &[u8])], exif: &[(u16, u16, &[u8])]) -> Vec<u8> {
        /// Writes one IFD's entries at `at`, their long values from `data_at`.
        fn ifd(entries: &[(u16, u16, &[u8])], at: u32, out: &mut Vec<u8>, data: &mut Vec<u8>) {
            let count = u32::try_from(entries.len()).unwrap();
            let data_at = at + 2 + 12 * count + 4;
            out.extend_from_slice(&u16::try_from(count).unwrap().to_le_bytes());
            for (tag, kind, value) in entries {
                out.extend_from_slice(&tag.to_le_bytes());
                out.extend_from_slice(&kind.to_le_bytes());
                // The count is of values: a LONG is 4 bytes, the rest here 1.
                let unit = if *kind == 4 { 4 } else { 1 };
                out.extend_from_slice(&u32::try_from(value.len() / unit).unwrap().to_le_bytes());
                if value.len() <= 4 {
                    let mut inline = [0u8; 4];
                    inline[..value.len()].copy_from_slice(value);
                    out.extend_from_slice(&inline);
                } else {
                    let offset = data_at + u32::try_from(data.len()).unwrap();
                    out.extend_from_slice(&offset.to_le_bytes());
                    data.extend_from_slice(value);
                }
            }
            out.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        }

        let mut jpeg = Vec::from(b"\xff\xd8".as_slice()); // SOI
        if !(ifd0.is_empty() && exif.is_empty()) {
            let mut entries = ifd0.to_vec();
            // IFD0 at 8, then its out-of-line values, then the Exif IFD and its.
            let mut tiff = Vec::from(b"II\x2a\x00\x08\x00\x00\x00".as_slice());
            let pointer;
            if !exif.is_empty() {
                let ifd0_len = 2 + 12 * (entries.len() + 1) + 4;
                let ifd0_data: usize = ifd0.iter().map(|e| e.2.len()).filter(|n| *n > 4).sum();
                pointer = u32::try_from(8 + ifd0_len + ifd0_data)
                    .unwrap()
                    .to_le_bytes();
                entries.push((0x8769, 4, &pointer));
            }
            let (mut out, mut data) = (Vec::new(), Vec::new());
            ifd(&entries, 8, &mut out, &mut data);
            tiff.extend_from_slice(&out);
            tiff.extend_from_slice(&data);
            if !exif.is_empty() {
                let (mut out, mut data) = (Vec::new(), Vec::new());
                ifd(
                    exif,
                    u32::try_from(tiff.len()).unwrap(),
                    &mut out,
                    &mut data,
                );
                tiff.extend_from_slice(&out);
                tiff.extend_from_slice(&data);
            }
            let mut app1 = Vec::from(b"Exif\0\0".as_slice());
            app1.extend_from_slice(&tiff);
            jpeg.extend_from_slice(b"\xff\xe1");
            jpeg.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap().to_be_bytes());
            jpeg.extend_from_slice(&app1);
        }
        // SOF0: 8-bit, 1x1, one component.
        jpeg.extend_from_slice(b"\xff\xc0\x00\x0b\x08\x00\x01\x00\x01\x01\x01\x11\x00");
        jpeg.extend_from_slice(b"\xff\xd9"); // EOI
        jpeg
    }

    /// gl-1611: the comment TagLib#'s `IFDTag.Comment` reads. Each case has the
    /// tags of a JPEG a stock Jellyfin 12.4 (TagLibSharp 2.3.0) scanned, and
    /// `expected` is the `Overview` it stored, which is this comment, or "" for
    /// `None`.
    #[rstest::rstest]
    #[case::no_exif(&[], &[], None)]
    #[case::an_empty_comment(&[], &[(0x9286, 7, b"ASCII\0\0\0".as_slice())], Some(""))]
    #[case::padding_is_kept(&[], &[(0x9286, 7, b"ASCII\0\0\0  both ends  ".as_slice())], Some("  both ends  "))]
    #[case::spaces_alone_are_kept(&[], &[(0x9286, 7, b"ASCII\0\0\0   ".as_slice())], Some("   "))]
    #[case::the_text_ends_at_a_nul(&[], &[(0x9286, 7, b"ASCII\0\0\0mid\0dle".as_slice())], Some("mid"))]
    #[case::trailing_nuls(&[], &[(0x9286, 7, b"ASCII\0\0\0nul padded\0\0\0".as_slice())], Some("nul padded"))]
    #[case::a_description_alone(&[(0x010e, 2, b"  padded desc  \0".as_slice())], &[], Some("  padded desc  "))]
    #[case::an_empty_comment_hides_the_description(
        &[(0x010e, 2, b"the description\0".as_slice())],
        &[(0x9286, 7, b"ASCII\0\0\0".as_slice())],
        Some("")
    )]
    #[case::undefined_zeros_hide_the_description(
        &[(0x010e, 2, b"canon desc\0".as_slice())],
        &[(0x9286, 7, [0u8; 40].as_slice())],
        Some("")
    )]
    #[case::spaces_without_a_code_hide_the_description(
        &[(0x010e, 2, b"spaces desc\0".as_slice())],
        &[(0x9286, 7, b"        ".as_slice())],
        Some("")
    )]
    #[case::a_byte_comment_is_no_comment(
        &[(0x010e, 2, b"canon desc\0".as_slice())],
        &[(0x9286, 1, [0u8; 40].as_slice())],
        Some("canon desc")
    )]
    #[case::an_ascii_comment_is_no_comment(
        &[(0x010e, 2, b"typed desc\0".as_slice())],
        &[(0x9286, 2, b"ascii typed\0".as_slice())],
        Some("typed desc")
    )]
    #[case::a_four_byte_comment_is_no_comment(
        &[(0x010e, 2, b"short desc\0".as_slice())],
        &[(0x9286, 7, b"ab\0\0".as_slice())],
        Some("short desc")
    )]
    #[case::the_undefined_code(&[], &[(0x9286, 7, b"\0\0\0\0\0\0\0\0undefined text".as_slice())], Some("undefined text"))]
    #[case::the_jis_code_is_read_as_text(&[], &[(0x9286, 7, b"JIS\0\0\0\0\0jis text".as_slice())], Some("JIS"))]
    #[case::a_corrupt_unicode_code(&[], &[(0x9286, 7, b"Unicode\0bad code\0x".as_slice())], Some("bad code"))]
    #[case::an_empty_unicode_comment_hides_the_description(
        &[(0x010e, 2, b"the description\0".as_slice())],
        &[(0x9286, 7, b"UNICODE\0".as_slice())],
        Some("")
    )]
    // Not upstream's value: TagLib reads UNICODE text as UTF-8 and stores ''
    // for this one (y10 in gl-1611's measurements). Ferrofin reads UTF-16,
    // as the EXIF specification means; the choice is the owner's (gl-1613).
    #[case::unicode_text_is_utf16(&[], &[(0x9286, 7, b"UNICODE\0\xff\xfeH\0i\0".as_slice())], Some("Hi"))]
    fn the_comment_is_taglibs(
        #[case] ifd0: &[(u16, u16, &[u8])],
        #[case] exif: &[(u16, u16, &[u8])],
        #[case] expected: Option<&str>,
    ) {
        let (read, _) = streamed_tags(&jpeg_with_tags(ifd0, exif));
        assert_eq!(read.and_then(|tags| tags.comment).as_deref(), expected);
    }

    /// A PNG of one pixel with no EXIF: its signature, IHDR and IEND. The CRCs
    /// are right, as TagLib checks the IHDR's.
    fn png_without_exif() -> Vec<u8> {
        let mut png = Vec::from(b"\x89PNG\r\n\x1a\n".as_slice());
        png.extend_from_slice(b"\0\0\0\x0dIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0\x90\x77\x53\xde");
        png.extend_from_slice(b"\0\0\0\0IEND\xae\x42\x60\x82");
        png
    }

    /// A one-pixel TIFF with no comment. TagLib needs its dimensions.
    fn tiff_without_a_comment() -> Vec<u8> {
        let one = vec![1, 0, 0, 0];
        let dimensions = [(0x0100, 3, 1, one.clone()), (0x0101, 3, 1, one)];
        tiff_file(true, 0, &dimensions, &[], &[], &[])
    }

    /// A one-pixel Canon CR2 with no comment: a TIFF whose header goes on with
    /// "CR", version 2.0 and the RAW IFD's offset. TagLib takes a CR2's
    /// dimensions from the Exif IFD.
    fn cr2_without_a_comment() -> Vec<u8> {
        let one = vec![1, 0, 0, 0];
        let dimensions = [(0xa002, 4, 1, one.clone()), (0xa003, 4, 1, one)];
        let mut cr2 = tiff_file(true, 8, &[], &dimensions, &[], &[]);
        cr2[8..16].copy_from_slice(b"CR\x02\x00\0\0\0\0");
        cr2
    }

    /// gl-1611: what a photo's `Overview` becomes, measured against a stock
    /// Jellyfin 12.4 for each shape. A file TagLib opens stores its comment, ""
    /// when it holds none (TagLib's combined comment is never null). A file it
    /// cannot open is left alone: a WebP or an AVIF, which it has no reader
    /// for, a file that does not start as its extension's format does, and a
    /// broken one.
    #[rstest::rstest]
    #[case::a_jpeg_without_exif("photo.jpg", jpeg_with_tags(&[], &[]), Some(""))]
    #[case::an_empty_comment(
        "photo.JPEG",
        jpeg_with_tags(&[], &[(0x9286, 7, b"ASCII\0\0\0".as_slice())]),
        Some("")
    )]
    #[case::a_png_without_exif("photo.png", png_without_exif(), Some(""))]
    #[case::a_tiff_without_a_comment("photo.tiff", tiff_without_a_comment(), Some(""))]
    #[case::a_cr2_without_a_comment("photo.cr2", cr2_without_a_comment(), Some(""))]
    #[case::a_webp_without_exif("photo.webp", jpeg_with_tags(&[], &[]), None)]
    #[case::an_avif_with_an_empty_comment(
        "photo.avif",
        jpeg_with_tags(&[], &[(0x9286, 7, b"ASCII\0\0\0".as_slice())]),
        None
    )]
    // Not upstream's value, which is NULL: Ferrofin reads a WebP's EXIF until
    // gl-1614 decides.
    #[case::a_webp_with_a_comment(
        "photo.webp",
        jpeg_with_tags(&[], &[(0x9286, 7, b"ASCII\0\0\0hi".as_slice())]),
        Some("hi")
    )]
    #[case::an_empty_file("photo.jpg", Vec::new(), Some("stale"))]
    #[case::not_an_image("photo.jpg", b"<html>not an image</html>".to_vec(), Some("stale"))]
    #[case::a_png_named_jpg("photo.jpg", png_without_exif(), Some("stale"))]
    #[case::a_jpeg_named_png("photo.png", jpeg_with_tags(&[], &[]), Some("stale"))]
    #[case::a_png_named_tiff("photo.tiff", png_without_exif(), Some("stale"))]
    #[case::a_tiff_named_png("photo.png", tiff_without_a_comment(), Some("stale"))]
    #[case::a_tiff_named_cr2("photo.cr2", tiff_without_a_comment(), Some("stale"))]
    #[case::a_broken_exif_block(
        "photo.jpg",
        b"\xff\xd8\xff\xe1\x00\x1cExif\0\0\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\xff\xd9".to_vec(),
        Some("stale")
    )]
    #[case::cut_inside_a_segment("photo.jpg", b"\xff\xd8\xff\xe0\x00\x10JFIF".to_vec(), Some("stale"))]
    #[tokio::test]
    async fn the_overview_is_taglibs_comment(
        #[case] name: &str,
        #[case] bytes: Vec<u8>,
        #[case] expected: Option<&str>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_temp(&dir, name, &bytes);
        let provider = PhotoProvider::new(Arc::new(FakeProcessor::reporting(0, 0)));
        let mut item = PhotoItem {
            path,
            width: 1,
            height: 1,
            overview: Some("stale".into()),
            ..Default::default()
        };
        provider.fetch(&mut item).await.expect("fetch");
        assert_eq!(item.exif_was_read, expected != Some("stale"));
        assert_eq!(item.overview.as_deref(), expected);
    }
}
