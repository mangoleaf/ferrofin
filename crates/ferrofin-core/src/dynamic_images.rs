//! Dynamic image providers — the collages Jellyfin composes for items that have
//! no artwork of their own.
//!
//! Port of `Emby.Server.Implementations/Images/BaseDynamicImageProvider.cs` and
//! the concrete providers deriving from it:
//!
//! | C# provider                 | Item kind    | Sources (`GetItemsWithImages`)                                   |
//! |-----------------------------|--------------|-------------------------------------------------------------------|
//! | `GenreImageProvider`        | `Genre`      | 4 random `Series`/`Movie` carrying the genre, with a Primary      |
//! | `MusicGenreImageProvider`   | `MusicGenre` | 4 random `MusicAlbum`/`MusicVideo`/`Audio` with the genre + Primary |
//! | `PlaylistImageProvider`     | `Playlist`   | the playlist's members (episode → its series, song → its album)   |
//! | `PhotoAlbumImageProvider`   | `PhotoAlbum` | the album's first child with a Primary (`BaseFolderImageProvider`) |
//! | `ArtistImageProvider`       | `MusicArtist`| none — upstream returns `Array.Empty` (see [`ArtistSources`])       |
//!
//! `CollectionFolderImageProvider` (the library tiles) is ported separately by
//! the scanner's `refresh_library_images`, and `DynamicImageProvider`
//! (`UserView`) has nothing to act on here: Ferrofin never persists `UserView`
//! rows — a user's views *are* the `CollectionFolder` rows, whose tile that
//! same pass already draws.
//!
//! Every provider here supports only `ImageType.Primary`, and the base class
//! routes all five kinds to `CreateSquareCollage` — a
//! [`SQUARE_COLLAGE_SIZE`]-pixel square 2×2 tile — except the photo album,
//! whose `BaseFolderImageProvider` copies the single source image verbatim
//! (`CreateSingleImage`). The source image preference is upstream's
//! `GetStripCollageImagePaths`: Primary, then Thumb (the Backdrop-first rule is
//! reserved for `CollectionFolder`/`UserView`).
//!
//! # When an image is (re)generated
//!
//! `BaseDynamicImageProvider.HasChanged` + `FetchAsync`, per item:
//!
//! - no Primary yet → generate iff the provider finds at least one source;
//! - a Primary that is a remote URL, or a local file **outside** the item's
//!   internal metadata folder (a user upload / local `folder.jpg`) → never
//!   touched;
//! - a generated Primary (inside the metadata folder) → regenerated only when
//!   the file's modification time no longer matches the recorded
//!   `DateModified` (`HasChangedByDate`), or when the refresh is forced
//!   (`FullRefresh` / `ReplaceAllMetadata`, [`DynamicImageProviders::refresh_item`]
//!   with `force`).
//!
//! The pass runs at the end of every library scan over every supported row
//! ([`DynamicImageProviders::refresh_all`]). Upstream's `GenresValidator`
//! only refreshes genres it is seeing for the first time, so a genre whose
//! movies gained posters *after* it was created stays blank there until a
//! manual refresh; Ferrofin re-evaluates the (cheap) `HasChanged` gate each
//! scan instead, and the two converge on the same image once one exists.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities::ImageType;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::drawing::ImageProcessor;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{ImageCollageOptions, InternalItemsQuery, ItemImageInfo};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::providers::ItemUpdateType;
use uuid::Uuid;

use crate::item_type_lookup::kind_from_type_name;

/// Edge length of the square collage every provider here draws — upstream's
/// `BaseDynamicImageProvider.CreateSquareCollage` (`600, 600`).
pub const SQUARE_COLLAGE_SIZE: i32 = 600;

/// How many source items the genre providers sample —
/// `GenreImageProvider`/`MusicGenreImageProvider` (`Limit = 4`).
pub const GENRE_COLLAGE_SOURCES: i32 = 4;

/// The square collage is a 2×2 grid (`StripCollageBuilder.BuildSquareCollage`),
/// so four source images fill it; collecting more changes nothing.
const SQUARE_COLLAGE_TILES: usize = 4;

/// The image file every provider writes (`Path.ChangeExtension(…, ".png")` in
/// `CreateImage`, stored under the metadata folder as Jellyfin's `SaveImage`
/// names a Primary: `primary.<ext>`).
const PRIMARY_STEM: &str = "primary";

/// The image file extensions `SaveImage` may have left behind for an earlier
/// Primary of the same item; a regeneration replaces all of them.
const ART_FILE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif"];

/// Which dynamic image provider applies to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicImageKind {
    /// `GenreImageProvider`.
    Genre,
    /// `MusicGenreImageProvider`.
    MusicGenre,
    /// `PlaylistImageProvider`.
    Playlist,
    /// `PhotoAlbumImageProvider`.
    PhotoAlbum,
    /// `ArtistImageProvider` — supported, but upstream gives it no sources.
    MusicArtist,
}

impl DynamicImageKind {
    /// The provider for a stored row, by its `BaseItems.Type`; `None` for
    /// kinds no dynamic provider `Supports`.
    #[must_use]
    pub fn for_entity(entity: &BaseItemEntity) -> Option<Self> {
        match kind_from_type_name(&entity.type_)? {
            BaseItemKind::Genre => Some(Self::Genre),
            BaseItemKind::MusicGenre => Some(Self::MusicGenre),
            BaseItemKind::Playlist => Some(Self::Playlist),
            BaseItemKind::PhotoAlbum => Some(Self::PhotoAlbum),
            BaseItemKind::MusicArtist => Some(Self::MusicArtist),
            _ => None,
        }
    }
}

/// Port of `ArtistImageProvider.GetItemsWithImages`: it returns
/// `Array.Empty<BaseItem>()` — the album-sampling query is commented out
/// upstream ("enable this when `BaseDynamicImageProvider` objects are
/// configurable") — so an artist never gets a generated Primary. Kept as a
/// named, tested no-op so the port is explicit rather than an omission.
pub struct ArtistSources;

impl ArtistSources {
    /// The artist's collage sources: always none.
    #[must_use]
    pub fn image_paths() -> Vec<String> {
        Vec::new()
    }
}

/// What one pass over the library produced.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DynamicImageReport {
    /// Rows the pass looked at.
    pub examined: usize,
    /// Rows whose Primary was (re)generated.
    pub generated: usize,
}

/// The dynamic image providers over the item repository + persistence seams.
///
/// One value serves every kind; [`refresh_all`](Self::refresh_all) is the
/// scan-time pass and [`refresh_item`](Self::refresh_item) the single-item
/// `FetchAsync`.
pub struct DynamicImageProviders {
    items: Arc<dyn ItemRepository>,
    persistence: Arc<dyn ItemPersistenceService>,
    processor: Arc<dyn ImageProcessor>,
    /// The item-art root (`{metadata}/library`); an item's folder is
    /// `{root}/{ID}`, the same layout the scanner's downloads and the
    /// image-upload endpoint use, so an upload of another type survives and a
    /// rescan re-adopts the generated file.
    metadata_dir: PathBuf,
}

impl std::fmt::Debug for DynamicImageProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicImageProviders")
            .field("metadata_dir", &self.metadata_dir)
            .finish_non_exhaustive()
    }
}

impl DynamicImageProviders {
    /// Wires the providers over the repositories, the image processor that
    /// composites the collage, and the item-art root.
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        persistence: Arc<dyn ItemPersistenceService>,
        processor: Arc<dyn ImageProcessor>,
        metadata_dir: PathBuf,
    ) -> Self {
        Self {
            items,
            persistence,
            processor,
            metadata_dir,
        }
    }

    /// The scan-time pass: evaluates every supported row and (re)generates
    /// the Primaries whose `HasChanged` gate opens.
    ///
    /// Cost is bounded by the gate: a row with a current generated image costs
    /// one image-row read and one `stat`; only rows without a Primary run their
    /// provider's (`LIMIT`ed) source query. Per-row failures are logged and
    /// skipped — one unreadable poster must not lose every other collage.
    ///
    /// Genre rows go first: Ferrofin materializes one `Genre` row per name
    /// shared by the Genres and Music Genres tabs (ids are the `ItemValueId`),
    /// so a name used by both a movie and an album has a single Primary —
    /// the video collage when the genre has any video sources, the music one
    /// otherwise. Upstream keeps two rows and two images.
    ///
    /// # Errors
    ///
    /// Only when a supported kind's row list cannot be read; a failure on one
    /// row is logged and the pass continues.
    pub async fn refresh_all(&self) -> Result<DynamicImageReport, ServiceError> {
        if !self.processor.supports_image_collage_creation() {
            return Ok(DynamicImageReport::default());
        }
        let mut report = DynamicImageReport::default();
        for kind in [
            BaseItemKind::Genre,
            BaseItemKind::MusicGenre,
            BaseItemKind::Playlist,
            BaseItemKind::PhotoAlbum,
        ] {
            let rows = self
                .items
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    ..InternalItemsQuery::default()
                })
                .await?;
            for row in rows {
                report.examined += 1;
                match self.refresh_item(&row, false).await {
                    Ok(ItemUpdateType::ImageUpdate) => report.generated += 1,
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(%err, item_id = %row.id, kind = %row.type_, "dynamic image failed");
                    }
                }
            }
        }
        tracing::debug!(
            examined = report.examined,
            generated = report.generated,
            "dynamic image pass finished"
        );
        Ok(report)
    }

    /// Port of `BaseDynamicImageProvider.FetchAsync` for one item: returns
    /// [`ItemUpdateType::ImageUpdate`] when a Primary was written, otherwise
    /// [`ItemUpdateType::None`].
    ///
    /// `force` is the metadata service's `runAllProviders` (a `FullRefresh` /
    /// `ReplaceAllMetadata` refresh): it skips the `HasChanged` date gate and
    /// resamples, but still never overwrites an image the provider did not
    /// create.
    ///
    /// # Errors
    ///
    /// A repository read/write failure, an unwritable art folder, or a collage
    /// the image processor could not composite.
    pub async fn refresh_item(
        &self,
        item: &BaseItemEntity,
        force: bool,
    ) -> Result<ItemUpdateType, ServiceError> {
        let Some(kind) = DynamicImageKind::for_entity(item) else {
            return Ok(ItemUpdateType::None);
        };
        let Ok(item_id) = Uuid::parse_str(&item.id) else {
            return Ok(ItemUpdateType::None);
        };
        let mut images = self.items.get_image_infos(item_id).await?;
        let item_dir = self.item_dir(item_id);
        if let Some(primary) = images.iter().find(|i| i.image_type == ImageType::Primary) {
            // `FetchAsync`: an image that is not ours is never replaced.
            if !primary.is_local_file() || !is_under(&item_dir, Path::new(&primary.path)) {
                return Ok(ItemUpdateType::None);
            }
            // `HasChanged` → `HasChangedByDate`: unchanged on disk means done.
            if !force && !has_changed_by_date(primary) {
                return Ok(ItemUpdateType::None);
            }
        }

        // Genre rows serve both tabs (see `refresh_all`): a genre with no video
        // sources falls through to the music sampling.
        let mut sources = self.sources_for(kind, item, item_id).await?;
        if sources.is_empty() && kind == DynamicImageKind::Genre {
            sources = self
                .sources_for(DynamicImageKind::MusicGenre, item, item_id)
                .await?;
        }
        if sources.is_empty() {
            // `CreateImage` returns null for an empty set → `ItemUpdateType.None`.
            return Ok(ItemUpdateType::None);
        }

        std::fs::create_dir_all(&item_dir).map_err(|e| {
            ServiceError::backend(format!(
                "failed to create the item art dir {}: {e}",
                item_dir.display()
            ))
        })?;
        let output = if kind == DynamicImageKind::PhotoAlbum {
            copy_single_image(&item_dir, &sources[0])?
        } else {
            let output = item_dir.join(format!("{PRIMARY_STEM}.png"));
            let options = ImageCollageOptions {
                input_paths: sources,
                output_path: output.to_string_lossy().into_owned(),
                width: SQUARE_COLLAGE_SIZE,
                height: SQUARE_COLLAGE_SIZE,
            };
            remove_other_primaries(&item_dir, &output);
            self.processor
                .create_image_collage(&options, item.name.as_deref())
                .await?;
            output
        };

        // `ProviderManager.SaveImage`: the new file replaces the Primary row
        // (other image types stay), recorded with the file's own modification
        // time so the next `HasChangedByDate` sees it as current.
        let mut info = ItemImageInfo {
            path: output.to_string_lossy().into_owned(),
            image_type: ImageType::Primary,
            date_modified: file_date_modified(&output),
            width: 0,
            height: 0,
            blur_hash: None,
        };
        self.fill_image_metadata(&mut info).await;
        images.retain(|i| i.image_type != ImageType::Primary);
        images.insert(0, info);
        self.persistence.save_item_images(item_id, &images).await?;
        tracing::debug!(item_id = %item.id, kind = ?kind, "dynamic image generated");
        Ok(ItemUpdateType::ImageUpdate)
    }

    /// The item's art folder (`{root}/{ID}`).
    fn item_dir(&self, item_id: Uuid) -> PathBuf {
        self.metadata_dir.join(guid_to_db(item_id))
    }

    /// `GetItemsWithImages` + `GetStripCollageImagePaths` for one provider: the
    /// local image file paths the collage is built from, in source order.
    async fn sources_for(
        &self,
        kind: DynamicImageKind,
        item: &BaseItemEntity,
        item_id: Uuid,
    ) -> Result<Vec<String>, ServiceError> {
        match kind {
            DynamicImageKind::Genre => {
                self.genre_sources(item, &[BaseItemKind::Series, BaseItemKind::Movie])
                    .await
            }
            DynamicImageKind::MusicGenre => {
                self.genre_sources(
                    item,
                    &[
                        BaseItemKind::MusicAlbum,
                        BaseItemKind::MusicVideo,
                        BaseItemKind::Audio,
                    ],
                )
                .await
            }
            DynamicImageKind::Playlist => self.playlist_sources(item_id).await,
            DynamicImageKind::PhotoAlbum => self.photo_album_sources(item_id).await,
            DynamicImageKind::MusicArtist => Ok(ArtistSources::image_paths()),
        }
    }

    /// `GenreImageProvider` / `MusicGenreImageProvider.GetItemsWithImages`:
    /// `Genres = [name]`, `IncludeItemTypes = kinds`, `OrderBy Random`,
    /// `Limit = 4`, `Recursive`, `ImageTypes = [Primary]`.
    async fn genre_sources(
        &self,
        item: &BaseItemEntity,
        kinds: &[BaseItemKind],
    ) -> Result<Vec<String>, ServiceError> {
        let Some(name) = item.name.as_deref().filter(|n| !n.trim().is_empty()) else {
            return Ok(Vec::new());
        };
        let ids = self
            .items
            .get_item_ids(&InternalItemsQuery {
                genres: vec![name.to_owned()],
                include_item_types: kinds.to_vec(),
                order_by: vec![(ItemSortBy::Random, SortOrder::Ascending)],
                limit: Some(GENRE_COLLAGE_SOURCES),
                recursive: true,
                image_types: vec![ImageType::Primary],
                ..InternalItemsQuery::default()
            })
            .await?;
        self.strip_collage_paths(&ids).await
    }

    /// `PlaylistImageProvider.GetItemsWithImages`: for each member, an episode
    /// contributes its series (when that has a Primary), any item with a
    /// Primary contributes itself, and a song without one contributes its
    /// album; distinct by id, in playlist order.
    async fn playlist_sources(&self, playlist_id: Uuid) -> Result<Vec<String>, ServiceError> {
        let members = self
            .items
            .get_item_list(&InternalItemsQuery {
                parent_id: playlist_id,
                ..InternalItemsQuery::default()
            })
            .await?;
        let mut seen = std::collections::HashSet::new();
        let mut paths = Vec::new();
        for member in members {
            let Some(id) = self.playlist_member_source(&member).await? else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            if let Some(path) = self.primary_or_thumb_path(id).await? {
                paths.push(path);
            }
            // Only four tiles fit a square collage: the first four distinct
            // sources are the whole output, so the walk stops there.
            if paths.len() >= SQUARE_COLLAGE_TILES {
                break;
            }
        }
        Ok(paths)
    }

    /// The item a playlist member contributes to the collage, if any.
    async fn playlist_member_source(
        &self,
        member: &BaseItemEntity,
    ) -> Result<Option<Uuid>, ServiceError> {
        let Ok(member_id) = Uuid::parse_str(&member.id) else {
            return Ok(None);
        };
        let member_kind = kind_from_type_name(&member.type_);
        if member_kind == Some(BaseItemKind::Episode)
            && let Some(series) = member
                .series_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())
            && self.has_primary(series).await?
        {
            return Ok(Some(series));
        }
        if self.has_primary(member_id).await? {
            return Ok(Some(member_id));
        }
        // `GetOwner() ?? GetParent()`, accepted only when it is a MusicAlbum.
        let parent = member
            .owner_id
            .as_deref()
            .or(member.parent_id.as_deref())
            .and_then(|s| Uuid::parse_str(s).ok());
        if let Some(parent_id) = parent
            && let Some(parent) = self.items.retrieve_item(parent_id).await?
            && kind_from_type_name(&parent.type_) == Some(BaseItemKind::MusicAlbum)
            && self.has_primary(parent_id).await?
        {
            return Ok(Some(parent_id));
        }
        Ok(None)
    }

    /// `BaseFolderImageProvider.GetItemsWithImages`: the album's first
    /// descendant with a Primary — files before folders, then by sort name.
    async fn photo_album_sources(&self, album_id: Uuid) -> Result<Vec<String>, ServiceError> {
        let ids = self
            .items
            .get_item_ids(&InternalItemsQuery {
                parent_id: album_id,
                recursive: true,
                image_types: vec![ImageType::Primary],
                order_by: vec![
                    (ItemSortBy::IsFolder, SortOrder::Ascending),
                    (ItemSortBy::SortName, SortOrder::Ascending),
                ],
                limit: Some(1),
                ..InternalItemsQuery::default()
            })
            .await?;
        // `CreateSingleImage` takes the Primary only (no Thumb fallback) and
        // needs a file extension to carry over.
        let mut paths = Vec::new();
        for id in ids {
            let infos = self.items.get_image_infos(id).await?;
            if let Some(primary) = infos
                .iter()
                .find(|i| i.image_type == ImageType::Primary && i.is_local_file())
                && Path::new(&primary.path).extension().is_some()
            {
                paths.push(primary.path.clone());
            }
        }
        Ok(paths)
    }

    /// `GetStripCollageImagePaths` over a sampled id list: each item's local
    /// Primary, else its local Thumb; items with neither drop out.
    async fn strip_collage_paths(&self, ids: &[Uuid]) -> Result<Vec<String>, ServiceError> {
        let mut paths = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(path) = self.primary_or_thumb_path(*id).await? {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    /// The item's local Primary path, else its local Thumb path.
    async fn primary_or_thumb_path(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        let infos = self.items.get_image_infos(id).await?;
        let pick = |kind: ImageType| {
            infos
                .iter()
                .find(|i| i.image_type == kind && i.is_local_file())
                .map(|i| i.path.clone())
        };
        Ok(pick(ImageType::Primary).or_else(|| pick(ImageType::Thumb)))
    }

    /// `HasImage(ImageType.Primary)`.
    async fn has_primary(&self, id: Uuid) -> Result<bool, ServiceError> {
        Ok(self
            .items
            .get_image_infos(id)
            .await?
            .iter()
            .any(|i| i.image_type == ImageType::Primary))
    }

    /// Dimensions + blurhash for the written file, the way the scanner records
    /// every other image (so the DTO carries `Width`/`Height`/`ImageBlurHashes`).
    /// Best-effort: a decode failure leaves 0×0 / no hash.
    async fn fill_image_metadata(&self, image: &mut ItemImageInfo) {
        let Ok(dims) = self.processor.get_image_dimensions(&image.path).await else {
            return;
        };
        image.width = dims.width;
        image.height = dims.height;
        if let Ok(hash) = self
            .processor
            .get_image_blur_hash_sized(&image.path, dims)
            .await
        {
            image.blur_hash = Some(hash);
        }
    }
}

/// `BaseDynamicImageProvider.HasChangedByDate`: the recorded `DateModified`
/// differs from the file's current modification time. A file that can no
/// longer be read counts as changed (the image is gone; regenerate).
///
/// Both sides are compared at the database's 100 ns tick precision — the
/// stored value was truncated to it on the way in, so a raw nanosecond `mtime`
/// would never compare equal and every scan would redraw every collage.
fn has_changed_by_date(image: &ItemImageInfo) -> bool {
    let Ok(meta) = std::fs::metadata(&image.path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    datetime_to_db(DateTime::<Utc>::from(modified)) != datetime_to_db(image.date_modified)
}

/// `FileSystem.ContainsSubPath(item.GetInternalMetadataPath(), image.Path)`.
fn is_under(dir: &Path, path: &Path) -> bool {
    path.starts_with(dir)
}

/// The file's modification time, or now when it cannot be read.
fn file_date_modified(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_or_else(|_| Utc::now(), DateTime::<Utc>::from)
}

/// `BaseDynamicImageProvider.CreateSingleImage`: copies the source into the
/// item's folder as `primary.<source extension>`, replacing any earlier Primary.
fn copy_single_image(item_dir: &Path, source: &str) -> Result<PathBuf, ServiceError> {
    let source = Path::new(source);
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| ServiceError::invalid_input("single-image source has no extension"))?;
    let output = item_dir.join(format!("{PRIMARY_STEM}.{ext}"));
    remove_other_primaries(item_dir, &output);
    std::fs::copy(source, &output).map_err(|e| {
        ServiceError::backend(format!(
            "failed to copy {} to {}: {e}",
            source.display(),
            output.display()
        ))
    })?;
    Ok(output)
}

/// Removes every `primary.<ext>` in `item_dir` other than `keep`, so a
/// regeneration under a different extension leaves one Primary file behind
/// (the rescan re-adopts files by stem and would otherwise find two).
fn remove_other_primaries(item_dir: &Path, keep: &Path) {
    for ext in ART_FILE_EXTENSIONS {
        let candidate = item_dir.join(format!("{PRIMARY_STEM}.{ext}"));
        if candidate != keep
            && candidate.exists()
            && let Err(err) = std::fs::remove_file(&candidate)
        {
            tracing::debug!(%err, path = %candidate.display(), "stale primary not removed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::item_type_lookup::stored_type_name;
    use crate::test_support::{image_info, item_repository_over, seed_images, test_db};
    use ferrofin_db::Database;
    use ferrofin_db::enums::ItemValueType;
    use ferrofin_drawing::image_encoder::ImageCrateEncoder;
    use ferrofin_drawing::processor::ImageProcessor as DrawingProcessor;
    use ferrofin_traits::persistence::LinkedChildrenService;

    /// Writes a small solid-colour PNG and returns its path.
    fn write_png(dir: &Path, name: &str, rgb: [u8; 3]) -> String {
        let mut img = image::RgbImage::new(24, 36);
        for px in img.pixels_mut() {
            *px = image::Rgb(rgb);
        }
        let path = dir.join(name);
        img.save(&path).expect("write png");
        path.to_string_lossy().into_owned()
    }

    struct Fixture {
        db: Database,
        tmp: tempfile::TempDir,
        providers: DynamicImageProviders,
        items: Arc<dyn ItemRepository>,
        persistence: Arc<FerrofinItemPersistenceService>,
    }

    async fn fixture() -> Fixture {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let items = item_repository_over(db.clone());
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let processor: Arc<dyn ImageProcessor> = Arc::new(DrawingProcessor::new(
            Arc::new(ImageCrateEncoder::new()),
            tmp.path().join("cache"),
        ));
        let providers = DynamicImageProviders::new(
            Arc::clone(&items),
            persistence.clone(),
            processor,
            tmp.path().join("metadata").join("library"),
        );
        Fixture {
            db,
            tmp,
            providers,
            items,
            persistence,
        }
    }

    impl Fixture {
        fn media_dir(&self) -> PathBuf {
            let dir = self.tmp.path().join("media");
            std::fs::create_dir_all(&dir).expect("media dir");
            dir
        }

        /// Seeds a row of `kind`, tagged with `genre` (materializing the
        /// by-name Genre row), with an optional poster as its Primary.
        async fn seed(
            &self,
            kind: BaseItemKind,
            name: &str,
            genre: Option<&str>,
            poster: Option<&str>,
        ) -> Uuid {
            let id = Uuid::new_v4();
            self.persistence
                .save_items(&[BaseItemEntity {
                    id: guid_to_db(id),
                    type_: stored_type_name(kind).expect("type name").to_owned(),
                    name: Some(name.to_owned()),
                    ..Default::default()
                }])
                .await
                .expect("seed item");
            if let Some(genre) = genre {
                self.persistence
                    .save_item_values(id, &[(i32::from(ItemValueType::Genre), genre.to_owned())])
                    .await
                    .expect("genre");
            }
            if let Some(poster) = poster {
                seed_images(
                    &self.db,
                    id,
                    &[image_info(ImageType::Primary, poster, None)],
                )
                .await;
            }
            id
        }

        async fn by_name(&self, kind: BaseItemKind, name: &str) -> BaseItemEntity {
            self.by_name_opt(kind, name)
                .await
                .expect("by-name row exists")
        }

        async fn by_name_opt(&self, kind: BaseItemKind, name: &str) -> Option<BaseItemEntity> {
            self.items
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    name: Some(name.to_owned()),
                    ..InternalItemsQuery::default()
                })
                .await
                .expect("query")
                .into_iter()
                .next()
        }

        async fn primary_of(&self, id: Uuid) -> Option<ItemImageInfo> {
            self.items
                .get_image_infos(id)
                .await
                .expect("images")
                .into_iter()
                .find(|i| i.image_type == ImageType::Primary)
        }
    }

    /// A genre with two movie posters gets a 600×600 Primary written into its
    /// metadata folder and registered on the row (`GET /Genres/{name}/Images/
    /// Primary` serves it from there).
    #[tokio::test(flavor = "multi_thread")]
    async fn genre_with_movie_posters_gets_a_square_collage() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let a = write_png(&media, "a-poster.png", [200, 20, 20]);
        let b = write_png(&media, "b-poster.png", [20, 20, 200]);
        fx.seed(BaseItemKind::Movie, "Heat", Some("Crime"), Some(&a))
            .await;
        fx.seed(BaseItemKind::Movie, "Ronin", Some("Crime"), Some(&b))
            .await;

        let report = fx.providers.refresh_all().await.expect("pass");
        assert_eq!(report.generated, 1, "{report:?}");

        let genre = fx.by_name(BaseItemKind::Genre, "Crime").await;
        let genre_id = Uuid::parse_str(&genre.id).expect("genre id");
        let primary = fx.primary_of(genre_id).await.expect("genre has a Primary");
        assert_eq!(
            Path::new(&primary.path),
            fx.tmp
                .path()
                .join("metadata/library")
                .join(guid_to_db(genre_id))
                .join("primary.png")
        );
        let decoded = image::open(&primary.path).expect("decodable png");
        assert_eq!((decoded.width(), decoded.height()), (600, 600));
        assert_eq!((primary.width, primary.height), (600, 600));
        assert!(primary.blur_hash.as_deref().is_some_and(|h| !h.is_empty()));
        // The tiles are the two posters, laid out 2×2: the top-left cell is
        // one poster's colour and the canvas is not blank.
        let rgb = decoded.to_rgb8();
        let tl = rgb.get_pixel(10, 10).0;
        assert!(tl == [200, 20, 20] || tl == [20, 20, 200], "{tl:?}");
    }

    /// A second pass over a current image is a no-op: the recorded
    /// `DateModified` matches the file, so `HasChangedByDate` is false.
    #[tokio::test(flavor = "multi_thread")]
    async fn fresh_generated_image_is_not_regenerated() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let a = write_png(&media, "a-poster.png", [200, 20, 20]);
        fx.seed(BaseItemKind::Series, "Lost", Some("Drama"), Some(&a))
            .await;
        assert_eq!(fx.providers.refresh_all().await.expect("pass").generated, 1);
        let genre = fx.by_name(BaseItemKind::Genre, "Drama").await;
        let genre_id = Uuid::parse_str(&genre.id).expect("id");
        let first = fx.primary_of(genre_id).await.expect("primary");
        let first_bytes = std::fs::read(&first.path).expect("read");

        let report = fx.providers.refresh_all().await.expect("pass");
        assert_eq!(report.generated, 0, "{report:?}");
        let again = fx.primary_of(genre_id).await.expect("primary");
        assert_eq!(again.date_modified, first.date_modified);
        assert_eq!(std::fs::read(&again.path).expect("read"), first_bytes);

        // A forced refresh (FullRefresh) redraws even a current image.
        let forced = fx
            .providers
            .refresh_item(&genre, true)
            .await
            .expect("forced");
        assert_eq!(forced, ItemUpdateType::ImageUpdate);
    }

    /// `HasChangedByDate`: a file modified out of band no longer matches the
    /// recorded `DateModified`, so the next pass regenerates it.
    #[tokio::test(flavor = "multi_thread")]
    async fn image_touched_on_disk_is_regenerated() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let a = write_png(&media, "a-poster.png", [200, 20, 20]);
        fx.seed(BaseItemKind::Movie, "Heat", Some("Crime"), Some(&a))
            .await;
        assert_eq!(fx.providers.refresh_all().await.expect("pass").generated, 1);
        let genre = fx.by_name(BaseItemKind::Genre, "Crime").await;
        let genre_id = Uuid::parse_str(&genre.id).expect("id");
        let primary = fx.primary_of(genre_id).await.expect("primary");

        // Rewrite the file a second into the future so the mtime differs.
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&primary.path)
            .expect("open")
            .set_modified(future)
            .expect("set mtime");
        assert!(has_changed_by_date(&primary));

        assert_eq!(fx.providers.refresh_all().await.expect("pass").generated, 1);
        let redrawn = fx.primary_of(genre_id).await.expect("primary");
        assert!(!has_changed_by_date(&redrawn));
    }

    /// No source carries an image → nothing is written and nothing is
    /// registered (`CreateImage` returns null → `ItemUpdateType.None`).
    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_is_written_without_sources() {
        let fx = fixture().await;
        fx.seed(BaseItemKind::Movie, "Heat", Some("Crime"), None)
            .await;
        let report = fx.providers.refresh_all().await.expect("pass");
        assert_eq!(report.generated, 0);
        assert!(report.examined >= 1);
        let genre = fx.by_name(BaseItemKind::Genre, "Crime").await;
        let genre_id = Uuid::parse_str(&genre.id).expect("id");
        assert!(fx.primary_of(genre_id).await.is_none());
        assert!(
            !fx.tmp
                .path()
                .join("metadata/library")
                .join(guid_to_db(genre_id))
                .exists()
        );
    }

    /// `MusicGenreImageProvider` samples the audio kinds: a genre carried only
    /// by songs/albums gets its collage from their covers — and the video
    /// provider (Series/Movie) finds nothing for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn music_genre_samples_audio_kinds() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let cover = write_png(&media, "cover.png", [10, 160, 10]);
        let album = fx
            .seed(
                BaseItemKind::MusicAlbum,
                "Kind of Blue",
                Some("Jazz"),
                Some(&cover),
            )
            .await;
        fx.seed(BaseItemKind::Audio, "So What", Some("Jazz"), Some(&cover))
            .await;
        // A music item's genre materializes ONLY as a `MusicGenre` row —
        // upstream keeps `Genre` and `MusicGenre` disjoint
        // (`LibraryManager.GetMusicGenre` vs `GetGenre`).
        assert!(
            fx.by_name_opt(BaseItemKind::Genre, "Jazz").await.is_none(),
            "a music genre must not also produce a plain Genre row"
        );
        let genre = fx.by_name(BaseItemKind::MusicGenre, "Jazz").await;

        let video = fx
            .providers
            .sources_for(DynamicImageKind::Genre, &genre, album)
            .await
            .expect("video sources");
        assert!(video.is_empty(), "{video:?}");
        let music = fx
            .providers
            .sources_for(DynamicImageKind::MusicGenre, &genre, album)
            .await
            .expect("music sources");
        assert_eq!(music.len(), 2);
        assert!(music.iter().all(|p| p == &cover));

        let report = fx.providers.refresh_all().await.expect("pass");
        // Exactly one by-name row carries "Jazz" — the `MusicGenre` row the
        // scanner materializes for a music item's genre — and it earns the
        // collage. `/MusicGenres` browses it; the Genres tab does not list it.
        assert_eq!(report.generated, 1);
        let genre_id = Uuid::parse_str(&genre.id).expect("id");
        let primary = fx.primary_of(genre_id).await.expect("music genre primary");
        let decoded = image::open(&primary.path).expect("png");
        assert_eq!((decoded.width(), decoded.height()), (600, 600));
        assert_eq!(decoded.to_rgb8().get_pixel(10, 10).0, [10, 160, 10]);
    }

    /// The genre query honours `ImageTypes = [Primary]`: a movie without a
    /// poster is not a source, a movie whose only art is a Thumb is not
    /// sampled either (the filter is on Primary), and the sample is capped at 4.
    #[tokio::test(flavor = "multi_thread")]
    async fn genre_sampling_requires_a_primary_and_caps_at_four() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let poster = write_png(&media, "poster.png", [1, 2, 3]);
        for i in 0..6 {
            fx.seed(
                BaseItemKind::Movie,
                &format!("M{i}"),
                Some("Action"),
                Some(&poster),
            )
            .await;
        }
        fx.seed(BaseItemKind::Movie, "Bare", Some("Action"), None)
            .await;
        let thumb_only = fx
            .seed(BaseItemKind::Movie, "ThumbOnly", Some("Action"), None)
            .await;
        seed_images(
            &fx.db,
            thumb_only,
            &[image_info(ImageType::Thumb, &poster, None)],
        )
        .await;
        let genre = fx.by_name(BaseItemKind::Genre, "Action").await;
        let sources = fx
            .providers
            .sources_for(DynamicImageKind::Genre, &genre, thumb_only)
            .await
            .expect("sources");
        assert_eq!(sources.len(), 4, "{sources:?}");
    }

    /// A Primary the provider did not create — a local file outside the item's
    /// metadata folder, or a remote URL — is never replaced.
    #[tokio::test(flavor = "multi_thread")]
    async fn foreign_primary_is_left_alone() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let poster = write_png(&media, "poster.png", [1, 2, 3]);
        let user_art = write_png(&media, "genre-folder.png", [9, 9, 9]);
        fx.seed(BaseItemKind::Movie, "Heat", Some("Crime"), Some(&poster))
            .await;
        let genre = fx.by_name(BaseItemKind::Genre, "Crime").await;
        let genre_id = Uuid::parse_str(&genre.id).expect("id");
        seed_images(
            &fx.db,
            genre_id,
            &[image_info(ImageType::Primary, &user_art, None)],
        )
        .await;

        assert_eq!(fx.providers.refresh_all().await.expect("pass").generated, 0);
        assert_eq!(fx.primary_of(genre_id).await.expect("kept").path, user_art);
        // Even forced.
        assert_eq!(
            fx.providers
                .refresh_item(&genre, true)
                .await
                .expect("forced"),
            ItemUpdateType::None
        );

        seed_images(
            &fx.db,
            genre_id,
            &[image_info(
                ImageType::Primary,
                "https://img.example/crime.png",
                None,
            )],
        )
        .await;
        assert_eq!(
            fx.providers
                .refresh_item(&genre, true)
                .await
                .expect("remote"),
            ItemUpdateType::None
        );
    }

    /// A regenerated Primary replaces only the Primary row — an uploaded
    /// Backdrop on the genre survives.
    #[tokio::test(flavor = "multi_thread")]
    async fn other_image_types_survive_generation() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let poster = write_png(&media, "poster.png", [1, 2, 3]);
        let backdrop = write_png(&media, "backdrop.png", [4, 5, 6]);
        fx.seed(BaseItemKind::Movie, "Heat", Some("Crime"), Some(&poster))
            .await;
        let genre = fx.by_name(BaseItemKind::Genre, "Crime").await;
        let genre_id = Uuid::parse_str(&genre.id).expect("id");
        seed_images(
            &fx.db,
            genre_id,
            &[image_info(ImageType::Backdrop, &backdrop, None)],
        )
        .await;

        assert_eq!(fx.providers.refresh_all().await.expect("pass").generated, 1);
        let images = fx.items.get_image_infos(genre_id).await.expect("images");
        assert_eq!(images.len(), 2);
        assert!(
            images
                .iter()
                .any(|i| i.image_type == ImageType::Backdrop && i.path == backdrop)
        );
        assert!(images.iter().any(|i| i.image_type == ImageType::Primary));
    }

    /// `PlaylistImageProvider`: members contribute themselves, an episode its
    /// series, a song its album; distinct by id.
    #[tokio::test(flavor = "multi_thread")]
    async fn playlist_sources_follow_episode_series_and_song_album() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let series_art = write_png(&media, "series.png", [1, 1, 1]);
        let album_art = write_png(&media, "album.png", [2, 2, 2]);
        let movie_art = write_png(&media, "movie.png", [3, 3, 3]);

        let series = fx
            .seed(BaseItemKind::Series, "Lost", None, Some(&series_art))
            .await;
        let album = fx
            .seed(
                BaseItemKind::MusicAlbum,
                "Kind of Blue",
                None,
                Some(&album_art),
            )
            .await;
        let movie = fx
            .seed(BaseItemKind::Movie, "Heat", None, Some(&movie_art))
            .await;
        let playlist = fx.seed(BaseItemKind::Playlist, "Mix", None, None).await;
        // Two episodes of the series (no art of their own) and two album tracks.
        let mut members = vec![movie];
        for n in 0..2 {
            let ep = Uuid::new_v4();
            fx.persistence
                .save_items(&[BaseItemEntity {
                    id: guid_to_db(ep),
                    type_: stored_type_name(BaseItemKind::Episode).unwrap().to_owned(),
                    name: Some(format!("E{n}")),
                    series_id: Some(guid_to_db(series)),
                    ..Default::default()
                }])
                .await
                .expect("episode");
            members.push(ep);
            let track = Uuid::new_v4();
            fx.persistence
                .save_items(&[BaseItemEntity {
                    id: guid_to_db(track),
                    type_: stored_type_name(BaseItemKind::Audio).unwrap().to_owned(),
                    name: Some(format!("T{n}")),
                    parent_id: Some(guid_to_db(album)),
                    ..Default::default()
                }])
                .await
                .expect("track");
            members.push(track);
        }
        // `upsert_linked_child` appends in call order (SortOrder = max + 1).
        let links =
            crate::linked_children_service::FerrofinLinkedChildrenService::new(fx.db.clone());
        for m in &members {
            links
                .upsert_linked_child(playlist, *m, 0)
                .await
                .expect("link");
        }

        let row = fx.items.retrieve_item(playlist).await.unwrap().unwrap();
        let mut sources = fx
            .providers
            .sources_for(DynamicImageKind::Playlist, &row, playlist)
            .await
            .expect("sources");
        sources.sort();
        let mut expected = vec![movie_art.clone(), series_art.clone(), album_art.clone()];
        expected.sort();
        assert_eq!(sources, expected);

        assert_eq!(fx.providers.refresh_all().await.expect("pass").generated, 1);
        let primary = fx.primary_of(playlist).await.expect("playlist primary");
        let decoded = image::open(&primary.path).expect("png");
        assert_eq!((decoded.width(), decoded.height()), (600, 600));
    }

    /// `PhotoAlbumImageProvider` (`BaseFolderImageProvider`): the first photo
    /// by sort name is copied verbatim, keeping its extension, as the album's
    /// Primary — and is not redone while current.
    #[tokio::test(flavor = "multi_thread")]
    async fn photo_album_copies_its_first_photo() {
        let fx = fixture().await;
        let media = fx.media_dir();
        let album = fx
            .seed(BaseItemKind::PhotoAlbum, "Iceland", None, None)
            .await;
        let mut img = image::RgbImage::new(8, 8);
        for px in img.pixels_mut() {
            *px = image::Rgb([7, 7, 7]);
        }
        let later = media.join("zebra.jpg");
        img.save(&later).expect("jpg");
        let first = media.join("aurora.jpg");
        img.save(&first).expect("jpg");
        for (name, path) in [("zebra", &later), ("aurora", &first)] {
            let id = Uuid::new_v4();
            fx.persistence
                .save_items(&[BaseItemEntity {
                    id: guid_to_db(id),
                    type_: stored_type_name(BaseItemKind::Photo).unwrap().to_owned(),
                    name: Some(name.to_owned()),
                    sort_name: Some(name.to_owned()),
                    parent_id: Some(guid_to_db(album)),
                    path: Some(path.to_string_lossy().into_owned()),
                    ..Default::default()
                }])
                .await
                .expect("photo");
            fx.persistence
                .set_ancestors(id, &[album])
                .await
                .expect("ancestors");
            seed_images(
                &fx.db,
                id,
                &[image_info(
                    ImageType::Primary,
                    &path.to_string_lossy(),
                    None,
                )],
            )
            .await;
        }

        assert_eq!(fx.providers.refresh_all().await.expect("pass").generated, 1);
        let primary = fx.primary_of(album).await.expect("album primary");
        assert!(primary.path.ends_with("primary.jpg"), "{}", primary.path);
        assert_eq!(
            std::fs::read(&primary.path).unwrap(),
            std::fs::read(&first).unwrap(),
            "the sort-name-first photo is the one copied"
        );
        assert_eq!(fx.providers.refresh_all().await.expect("pass").generated, 0);
    }

    /// `ArtistImageProvider` yields no sources upstream, so an artist row is
    /// never given a generated Primary.
    #[tokio::test(flavor = "multi_thread")]
    async fn artist_provider_generates_nothing() {
        let fx = fixture().await;
        let artist = fx
            .seed(BaseItemKind::MusicArtist, "Miles Davis", None, None)
            .await;
        let row = fx.items.retrieve_item(artist).await.unwrap().unwrap();
        assert_eq!(
            DynamicImageKind::for_entity(&row),
            Some(DynamicImageKind::MusicArtist)
        );
        assert!(ArtistSources::image_paths().is_empty());
        assert_eq!(
            fx.providers.refresh_item(&row, true).await.expect("artist"),
            ItemUpdateType::None
        );
        assert!(fx.primary_of(artist).await.is_none());
    }

    #[test]
    fn kinds_map_to_their_providers() {
        let row = |kind: BaseItemKind| BaseItemEntity {
            type_: stored_type_name(kind).unwrap().to_owned(),
            ..Default::default()
        };
        assert_eq!(
            DynamicImageKind::for_entity(&row(BaseItemKind::Genre)),
            Some(DynamicImageKind::Genre)
        );
        assert_eq!(
            DynamicImageKind::for_entity(&row(BaseItemKind::MusicGenre)),
            Some(DynamicImageKind::MusicGenre)
        );
        assert_eq!(
            DynamicImageKind::for_entity(&row(BaseItemKind::Playlist)),
            Some(DynamicImageKind::Playlist)
        );
        assert_eq!(
            DynamicImageKind::for_entity(&row(BaseItemKind::PhotoAlbum)),
            Some(DynamicImageKind::PhotoAlbum)
        );
        assert_eq!(
            DynamicImageKind::for_entity(&row(BaseItemKind::Movie)),
            None
        );
        assert!(is_under(
            Path::new("/m/lib/ID"),
            Path::new("/m/lib/ID/primary.png")
        ));
        assert!(!is_under(
            Path::new("/m/lib/ID"),
            Path::new("/m/lib/IDX/primary.png")
        ));
    }

    #[test]
    fn regeneration_removes_stale_primaries_of_other_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("primary.jpg"), b"old").unwrap();
        std::fs::write(dir.join("backdrop.jpg"), b"keep").unwrap();
        let keep = dir.join("primary.png");
        std::fs::write(&keep, b"new").unwrap();
        remove_other_primaries(dir, &keep);
        assert!(!dir.join("primary.jpg").exists());
        assert!(dir.join("backdrop.jpg").exists());
        assert!(keep.exists());
    }
}
