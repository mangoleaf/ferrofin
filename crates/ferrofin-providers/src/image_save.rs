//! The shared media-adjacent image writer used by scans and manual uploads.
//! Names, eligibility and read-only fallback follow ImageSaver at 4910aafa1a.
mod policy;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::datetime_to_db;
use ferrofin_model::configuration::{ImageSavingConvention, XbmcMetadataOptions};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{SelfWrite, SelfWriteGuard, VirtualFolderManager};
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_util::directory_path::DirectoryPath;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

/// A published image together with the outcome of removing the file it
/// replaced.
///
/// `ImageSaver.SaveImage` stores the new path on the item *before* it deletes
/// the previous file and lets any deletion error other than "not found"
/// escape; callers therefore persist [`image`](Self::image) first and then
/// surface [`cleanup`](Self::cleanup).
#[derive(Debug)]
pub struct SavedImage {
    /// The image to persist, at its published destination.
    pub image: ItemImageInfo,
    /// The failure to remove the previous file, when removal was attempted.
    pub cleanup: Option<ServiceError>,
}

impl SavedImage {
    fn clean(image: ItemImageInfo) -> Self {
        Self {
            image,
            cleanup: None,
        }
    }
}

/// An item whose artwork [`ImageFileSaver`] publishes beside it
/// ([`ImageFileSaver::beside_art`]), for "Replace all images".
#[derive(Debug)]
pub struct BesideArt {
    item: BaseItemEntity,
    series: Option<BaseItemEntity>,
    /// The owning library's images are its media: a home-video (and photo)
    /// library with photos enabled, `PhotoResolver`'s.
    photos: bool,
}

impl BesideArt {
    /// Whether `RemoveImages` takes `image` although it is outside the item's
    /// metadata folders. Upstream takes it wherever it is stored; Ferrofin
    /// takes a file only where the saver writes the item's art and would
    /// delete a replaced one (its destinations' folder, never the item's own
    /// file or a shared album cover), so nothing else in a media folder is
    /// touched. In a home-video (and photo) library no file goes: an image
    /// beside the item there can be one of its photos (`jacket.jpg` is an
    /// album's Primary and a photo alike). A remote image (`!IsLocalFile`)
    /// goes, with no file to delete.
    #[must_use]
    pub fn removes(&self, image: &ItemImageInfo) -> bool {
        if !image.is_local_file() {
            return true;
        }
        let path = Path::new(&image.path);
        !self.photos
            && path.is_absolute()
            && policy::eligible(&self.item, self.series.as_ref(), image.image_type)
            && policy::art_folder(&self.item, self.series.as_ref())
                .is_some_and(|folder| path.starts_with(folder))
            && Some(path) != self.item.path.as_deref().map(Path::new)
            && !shared(path)
    }
}

struct Selection<'a> {
    index: usize,
    current: &'a [ItemImageInfo],
    previous: &'a [ItemImageInfo],
}

struct Settings {
    series: Option<BaseItemEntity>,
    convention: ImageSavingConvention,
    extra_thumbs: bool,
}

/// Whether `path` lies in one of the server's own directories rather than in
/// a media library: the data directory (a playlist in `{data}/playlists`, a
/// box set in `{data}/collections`), the internal metadata directory (a
/// genre's `{metadata}/Genre/<name>`, a person's `{metadata}/People/…`) or the
/// libraries' root folder (a library's own `root/default/<name>`). The check
/// is lexical, so a path it cannot match falls back to the library's setting.
/// It decides where the always-true `IsSaveLocalMetadataEnabled` overrides
/// hold (owner decision 2026-10-17).
pub(crate) fn in_server_directories(
    paths: &dyn ferrofin_traits::system::ServerApplicationPaths,
    path: &Path,
) -> bool {
    [
        paths.data_path(),
        paths.internal_metadata_path(),
        paths.root_folder_path(),
    ]
    .iter()
    .any(|root| !root.is_empty() && path.starts_with(root))
}

/// Publishes acquired artwork beside media when the owning library requests it.
/// Inputs are complete staged images; an unwritable local destination retains
/// that internal image as the ordinary ImageSaver fallback.
pub struct ImageFileSaver {
    items: Arc<dyn ItemRepository>,
    folders: Arc<dyn VirtualFolderManager>,
    configuration: Arc<dyn ServerConfigurationManager>,
    metadata: DirectoryPath,
    /// Brackets the files written beside the media (`ILibraryMonitor`).
    /// `None` in unit tests.
    self_writes: Option<Arc<dyn SelfWriteGuard>>,
}

impl ImageFileSaver {
    /// Creates the image destination service from the live configuration seams.
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        folders: Arc<dyn VirtualFolderManager>,
        configuration: Arc<dyn ServerConfigurationManager>,
        metadata: DirectoryPath,
    ) -> Self {
        Self {
            items,
            folders,
            configuration,
            metadata,
            self_writes: None,
        }
    }

    /// Brackets each file this saver writes on `guard`, as upstream does
    /// (`ReportFileSystemChangeBeginning`/`Complete` in `ImageSaver.SaveImageToLocation` and its delete of the previous image), so the
    /// library monitor takes it for the server's own write. Called once by
    /// the composition root.
    #[must_use]
    pub fn with_self_writes(mut self, guard: Arc<dyn SelfWriteGuard>) -> Self {
        self.self_writes = Some(guard);
        self
    }

    /// Applies the saved destination to a newly acquired or manually uploaded image.
    ///
    /// # Errors
    /// Returns a repository/configuration failure or an unrecoverable duplicate
    /// output failure. A single unwritable media destination falls back internally.
    /// A failure to remove the replaced file is reported in
    /// [`SavedImage::cleanup`], with the new image already published.
    pub async fn save(
        &self,
        item: &BaseItemEntity,
        image: ItemImageInfo,
        index: usize,
        current: &[ItemImageInfo],
    ) -> Result<SavedImage, ServiceError> {
        let Some(settings) = self.settings(item).await? else {
            return Ok(SavedImage::clean(image));
        };
        self.publish(
            item,
            image,
            Selection {
                index,
                current,
                previous: current,
            },
            &settings,
        )
        .await
    }

    /// Applies destinations only to newly acquired images, preserving local art.
    ///
    /// # Errors
    /// Returns repository/configuration or duplicate-output failures after other
    /// images are published independently. Single-output writes retain fallback.
    pub async fn save_all(
        &self,
        item: &BaseItemEntity,
        images: &mut [ItemImageInfo],
        previous: &[ItemImageInfo],
    ) -> Result<(), ServiceError> {
        // An image the item already stores is no acquisition, as `ImageSaver`
        // writes only what a provider just saved: the same file at the same
        // time, at the database's 100 ns precision (a listing of the art
        // folder reads the file's nanoseconds, and no size yet). Comparing
        // whole rows published a generated copy beside the media on the scan
        // after it was drawn.
        let stored = |image: &ItemImageInfo| {
            previous.iter().any(|old| {
                old.image_type == image.image_type
                    && old.path == image.path
                    && datetime_to_db(old.date_modified) == datetime_to_db(image.date_modified)
            })
        };
        let acquired =
            |image: &ItemImageInfo| !stored(image) && self.managed(Path::new(&image.path));
        // A scan saves every item through here, most of them with no image
        // acquired this pass: their destinations are never asked for.
        if !images.iter().any(&acquired) {
            return Ok(());
        }
        let Some(settings) = self.settings(item).await? else {
            return Ok(());
        };
        let mut current = Vec::new();
        let mut failure = None;
        for image in images {
            let index = current
                .iter()
                .filter(|old: &&ItemImageInfo| old.image_type == image.image_type)
                .count();
            if acquired(image) {
                match self
                    .publish(
                        item,
                        image.clone(),
                        Selection {
                            index,
                            current: &current,
                            previous,
                        },
                        &settings,
                    )
                    .await
                {
                    Ok(saved) => {
                        *image = saved.image;
                        if let Some(error) = saved.cleanup {
                            failure.get_or_insert(error);
                        }
                    }
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            current.push(image.clone());
        }
        failure.map_or(Ok(()), Err)
    }

    /// [`in_server_directories`] for the item's own path.
    fn in_server_directories(&self, item: &BaseItemEntity) -> bool {
        item.path.as_deref().is_some_and(|path| {
            in_server_directories(
                self.configuration.application_paths().as_ref(),
                Path::new(path),
            )
        })
    }

    /// `IsSaveLocalMetadataEnabled()` for `ItemImageProvider.RemoveImages`
    /// (`ItemImageProvider.cs:83-95`), by the rule [`save`](Self::save)
    /// publishes by: `Some` when the item's artwork is saved beside it, and
    /// then "Replace all images" removes the art saved there too
    /// ([`BesideArt::removes`]); `None` when it stays in the metadata folder.
    ///
    /// # Errors
    /// Returns a repository failure reading a season's series or the
    /// libraries.
    pub async fn beside_art(
        &self,
        item: &BaseItemEntity,
    ) -> Result<Option<BesideArt>, ServiceError> {
        let series = self.season_series(item).await?;
        if !self.saves_beside(item, series.as_ref()).await? {
            return Ok(None);
        }
        let photos = self
            .library(item, series.as_ref())
            .await?
            .is_some_and(|folder| {
                folder.collection_type == Some(CollectionTypeOptions::homevideos)
                    && folder
                        .library_options
                        .as_ref()
                        .is_none_or(|options| options.enable_photos)
            });
        Ok(Some(BesideArt {
            item: item.clone(),
            series,
            photos,
        }))
    }

    /// The item's library, from the libraries' snapshot, as upstream's
    /// `GetLibraryOptions(item)` reads them from memory: the dashboard's full
    /// listing re-heals and re-reads every library, once per saved item.
    async fn library(
        &self,
        item: &BaseItemEntity,
        series: Option<&BaseItemEntity>,
    ) -> Result<Option<VirtualFolderInfo>, ServiceError> {
        let folders = self.folders.library_folders().await?;
        Ok(ferrofin_model::entities_media::owning_library(
            &folders,
            item.top_parent_id.as_deref(),
            item.path
                .as_deref()
                .or_else(|| series.and_then(|series| series.path.as_deref())),
        )
        .cloned())
    }

    /// A season's series, whose folder its art is saved in.
    async fn season_series(
        &self,
        item: &BaseItemEntity,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        if policy::short_kind(item) != "Season" {
            return Ok(None);
        }
        let id = item
            .series_id
            .as_deref()
            .or(item.parent_id.as_deref())
            .and_then(|id| Uuid::parse_str(id).ok());
        match id {
            Some(id) => self.items.retrieve_item(id).await,
            None => Ok(None),
        }
    }

    /// Ferrofin's `IsSaveLocalMetadataEnabled()`: whether the item's artwork
    /// is published beside it.
    async fn saves_beside(
        &self,
        item: &BaseItemEntity,
        series: Option<&BaseItemEntity>,
    ) -> Result<bool, ServiceError> {
        // `IsSaveLocalMetadataEnabled`: these kinds override it to true
        // (`Genre.cs:55`, `MusicGenre.cs:63`, `Playlist.cs:126`, `Person.cs:89`,
        // `Studio.cs:62`, `Year.cs:54`, `BoxSet.cs:112`, `MusicArtist.cs:106`
        // when accessed by name, `CollectionFolder.cs:196`), so their artwork
        // lives beside their own path, out of reach of the dynamic image
        // providers. Accepted divergence (owner decision 2026-10-17): the
        // override holds only while that path is in the server's own
        // directories. A playlist file, a box-set folder or an artist folder
        // inside a media library follows the library's "Save artwork into
        // media folders", so nothing is written into a media folder the
        // library did not open to it. `UserView` and `BasePluginFolder` are
        // not given the override here (see gl-598).
        if matches!(
            policy::short_kind(item),
            "Genre"
                | "MusicGenre"
                | "Playlist"
                | "Person"
                | "Studio"
                | "Year"
                | "BoxSet"
                | "MusicArtist"
                | "CollectionFolder"
        ) && self.in_server_directories(item)
        {
            return Ok(true);
        }
        Ok(self
            .library(item, series)
            .await?
            .and_then(|folder| folder.library_options)
            .is_some_and(|options| options.save_local_metadata))
    }

    async fn settings(&self, item: &BaseItemEntity) -> Result<Option<Settings>, ServiceError> {
        let series = self.season_series(item).await?;
        if !self.saves_beside(item, series.as_ref()).await? {
            return Ok(None);
        }
        let configuration = self.configuration.configuration().await?;
        let path = PathBuf::from(
            self.configuration
                .application_paths()
                .configuration_directory_path(),
        )
        .join("xbmcmetadata.json");
        let extra_thumbs = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                serde_json::from_slice::<XbmcMetadataOptions>(&bytes)
                    .map_err(|error| ServiceError::backend(format!("read image options: {error}")))?
                    .enable_extra_thumbs_duplication
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(ServiceError::backend(format!(
                    "read image options: {error}"
                )));
            }
        };
        Ok(Some(Settings {
            series,
            convention: configuration.image_saving_convention,
            extra_thumbs,
        }))
    }

    fn managed(&self, path: &Path) -> bool {
        path.starts_with(self.metadata.resolve())
    }

    async fn publish(
        &self,
        item: &BaseItemEntity,
        image: ItemImageInfo,
        selection: Selection<'_>,
        settings: &Settings,
    ) -> Result<SavedImage, ServiceError> {
        if !self.managed(Path::new(&image.path))
            || !policy::eligible(item, settings.series.as_ref(), image.image_type)
        {
            return Ok(SavedImage::clean(image));
        }
        let extension = Path::new(&image.path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map_or_else(|| ".jpg".into(), |ext| format!(".{ext}"));
        let paths = policy::DestinationPolicy {
            item,
            series: settings.series.as_ref(),
            convention: settings.convention,
            extra_thumbs: settings.extra_thumbs,
            images: selection.current,
        }
        .destinations(image.image_type, &extension, selection.index);
        if paths.is_empty() {
            return Ok(SavedImage::clean(image));
        }
        let old = selection
            .previous
            .iter()
            .filter(|old| old.image_type == image.image_type)
            .nth(selection.index)
            .map(|old| PathBuf::from(&old.path))
            .filter(|old| {
                old.is_absolute()
                    && Some(old.as_path()) != item.path.as_deref().map(Path::new)
                    && !shared(old)
            });
        let source = PathBuf::from(&image.path);
        let destination = paths[0].clone();
        let duplicate = paths.len() > 1;
        let old_path = old.clone();
        let self_writes = self.self_writes.clone();
        let (copied, cleanup) = tokio::task::spawn_blocking(move || {
            let self_writes = self_writes.as_ref();
            match copy_images(&source, &paths, old.as_deref(), self_writes) {
                Ok(cleanup) => (Ok(()), cleanup),
                Err(error) => {
                    // The retained internal image is the new image, so the
                    // replaced file still goes.
                    let cleanup = (!duplicate && source.is_file())
                        .then(|| {
                            remove_previous(
                                old.as_deref(),
                                std::slice::from_ref(&source),
                                self_writes,
                            )
                        })
                        .and_then(Result::err);
                    (Err(error), cleanup)
                }
            }
        })
        .await
        .map_err(|error| ServiceError::backend(error.to_string()))?;
        let cleanup = cleanup.map(|error| {
            ServiceError::backend(format!(
                "remove replaced artwork {}: {error}",
                old_path
                    .as_deref()
                    .unwrap_or_else(|| Path::new(""))
                    .display()
            ))
        });
        match copied {
            Ok(()) => Ok(SavedImage {
                image: ItemImageInfo {
                    path: destination.to_string_lossy().into_owned(),
                    date_modified: std::fs::metadata(&destination)
                        .ok()
                        .and_then(|info| info.modified().ok())
                        .map_or(image.date_modified, chrono::DateTime::from),
                    ..image
                },
                cleanup,
            }),
            Err(error) if duplicate => Err(ServiceError::backend(format!(
                "write duplicated artwork {}: {error}",
                destination.display()
            ))),
            Err(error) => {
                tracing::warn!(%error,path=%destination.display(),"media artwork destination failed; retaining internal image");
                Ok(SavedImage { image, cleanup })
            }
        }
    }
}

fn shared(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str() == crate::provider_manager::SHARED_ALBUM_ARTWORK_DIR)
}

/// Writes every destination, then removes the replaced file and the staged
/// source. `Err` means a destination could not be written; `Ok(Some(_))` means
/// the images were published but the previous file could not be removed.
/// Each destination written is bracketed on `self_writes` with its folder, as
/// `ImageSaver.SaveImageToLocation` brackets both.
fn copy_images(
    source: &Path,
    paths: &[PathBuf],
    old: Option<&Path>,
    self_writes: Option<&Arc<dyn SelfWriteGuard>>,
) -> std::io::Result<Option<std::io::Error>> {
    let bytes = std::fs::read(source)?;
    for path in paths {
        if path != source && std::fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
            let _file = SelfWrite::begin(self_writes, path.to_string_lossy());
            let _folder = path
                .parent()
                .map(|folder| SelfWrite::begin(self_writes, folder.to_string_lossy()));
            ferrofin_util::file_helper::atomic_write_media(path, &bytes)?;
        }
    }
    let cleanup = remove_previous(old, paths, self_writes).err();
    if !paths.iter().any(|path| path == source) && !shared(source) {
        let _ = std::fs::remove_file(source);
    }
    Ok(cleanup)
}

/// `ImageSaver`'s "Delete the current path": skipped when any saved path equals
/// it ignoring case (`OrdinalIgnoreCase`), and only "file not found" is
/// forgiven — every other failure is returned. The delete is bracketed on
/// `self_writes`, as upstream brackets it.
fn remove_previous(
    old: Option<&Path>,
    paths: &[PathBuf],
    self_writes: Option<&Arc<dyn SelfWriteGuard>>,
) -> std::io::Result<()> {
    let Some(old) = old else {
        return Ok(());
    };
    let same = |path: &PathBuf| {
        ferrofin_util::string_extensions::equals_ordinal_ignore_case(
            &path.to_string_lossy(),
            &old.to_string_lossy(),
        )
    };
    if paths.iter().any(same) {
        return Ok(());
    }
    let _write = SelfWrite::begin(self_writes, old.to_string_lossy());
    match std::fs::remove_file(old) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::entities::ImageType::{Backdrop, Primary};
    /// A [`SelfWriteGuard`] recording each call, in order.
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<String>>);

    impl SelfWriteGuard for Recording {
        fn begin(&self, path: &str) {
            self.0.lock().unwrap().push(format!("begin {path}"));
        }
        fn complete(&self, path: &str, refresh_path: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("complete {path} {refresh_path}"));
        }
    }

    impl Recording {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// `ImageSaver` brackets each image it writes with its folder
    /// (`SaveImageToLocation`), and the previous image it deletes.
    #[test]
    fn publishing_brackets_each_destination_its_folder_and_the_replaced_file() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("internal.png");
        let folder = tmp.path().join("Movie");
        std::fs::create_dir(&folder).unwrap();
        let old = folder.join("folder.jpg");
        let dest = folder.join("folder.png");
        std::fs::write(&source, b"art").unwrap();
        std::fs::write(&old, b"old").unwrap();
        let recording = Arc::new(Recording::default());
        let guard: Arc<dyn SelfWriteGuard> = recording.clone();
        copy_images(
            &source,
            std::slice::from_ref(&dest),
            Some(&old),
            Some(&guard),
        )
        .unwrap();
        let (dest, folder, old) = (dest.display(), folder.display(), old.display());
        assert_eq!(
            recording.calls(),
            [
                format!("begin {dest}"),
                format!("begin {folder}"),
                format!("complete {folder} false"),
                format!("complete {dest} false"),
                format!("begin {old}"),
                format!("complete {old} false"),
            ]
        );
    }

    #[test]
    fn publishing_replaces_old_extension_and_preserves_identical_destination_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("internal.png");
        let old = tmp.path().join("folder.jpg");
        let dest = tmp.path().join("folder.png");
        std::fs::write(&source, b"art").unwrap();
        std::fs::write(&old, b"old").unwrap();
        assert!(
            copy_images(&source, std::slice::from_ref(&dest), Some(&old), None)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"art");
        assert!(!source.exists());
        assert!(!old.exists());
        let modified = std::fs::metadata(&dest).unwrap().modified().unwrap();
        std::fs::write(&source, b"art").unwrap();
        copy_images(&source, std::slice::from_ref(&dest), None, None).unwrap();
        assert_eq!(
            std::fs::metadata(&dest).unwrap().modified().unwrap(),
            modified
        );
    }
    /// `ImageSaver.SaveImage` forgives only "file not found" when deleting the
    /// previous image, compares saved paths ignoring case, and has already
    /// published the new image when the deletion fails.
    #[test]
    fn previous_file_failures_follow_the_saver_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("internal.png");
        let dest = tmp.path().join("folder.png");
        std::fs::write(&source, b"new").unwrap();
        // An undeletable previous file (a non-empty directory) is reported,
        // after the new image was written and the staged source removed.
        let stuck = tmp.path().join("old.jpg");
        std::fs::create_dir(&stuck).unwrap();
        std::fs::write(stuck.join("keep"), b"x").unwrap();
        let cleanup =
            copy_images(&source, std::slice::from_ref(&dest), Some(&stuck), None).unwrap();
        assert!(cleanup.is_some());
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!source.exists());
        // An already-absent previous file is fine.
        std::fs::write(&source, b"new").unwrap();
        let missing = tmp.path().join("gone.jpg");
        assert!(
            copy_images(&source, std::slice::from_ref(&dest), Some(&missing), None)
                .unwrap()
                .is_none()
        );
        // A previous path equal to a saved path ignoring case is kept.
        std::fs::write(&source, b"new").unwrap();
        let alias = tmp.path().join("FOLDER.PNG");
        std::fs::write(&alias, b"old").unwrap();
        assert!(
            copy_images(&source, std::slice::from_ref(&dest), Some(&alias), None)
                .unwrap()
                .is_none()
        );
        assert!(alias.exists());
    }

    #[test]
    fn failed_output_preserves_source_and_old_cover_and_shared_covers_remain_owned() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("internal.png");
        let old = tmp.path().join("old.png");
        let blocked = tmp.path().join("blocked.png");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&old, b"old").unwrap();
        std::fs::create_dir(&blocked).unwrap();
        assert!(copy_images(&source, std::slice::from_ref(&blocked), Some(&old), None).is_err());
        assert!(source.exists());
        assert!(old.exists());
        let shared = tmp.path().join("album-covers/album/content.png");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(&shared, b"shared").unwrap();
        let outputs = [
            tmp.path().join("extrafanart/fanart1.png"),
            tmp.path().join("extrathumbs/thumb1.png"),
        ];
        copy_images(&shared, &outputs, None, None).unwrap();
        assert!(shared.exists());
        for output in outputs {
            assert_eq!(std::fs::read(output).unwrap(), b"shared");
        }
    }

    /// A `kind` row at `path` (a folder when `is_folder`).
    fn row(kind: &str, path: &str, is_folder: bool) -> BaseItemEntity {
        BaseItemEntity {
            type_: format!("MediaBrowser.Controller.Entities.{kind}"),
            path: Some(path.into()),
            is_folder,
            ..Default::default()
        }
    }

    /// An image row of `image_type` at `path`.
    fn image(image_type: ferrofin_model::entities::ImageType, path: &str) -> ItemImageInfo {
        ItemImageInfo {
            path: path.into(),
            image_type,
            date_modified: chrono::DateTime::UNIX_EPOCH,
            width: 0,
            height: 0,
            blur_hash: None,
        }
    }

    /// gl-1741: with the item's art saved beside it, "Replace all images"
    /// takes an image from where the saver writes the item's art — its
    /// folder, a season's series folder — and a remote one; never the item's
    /// own file, a shared album cover, a file elsewhere, a relative path, or
    /// an image of a kind or type the saver never publishes beside the media.
    #[rstest::rstest]
    #[case::beside_a_box_set(
        "BoxSet",
        "/data/collections/Mix [boxset]",
        true,
        Primary,
        "/data/collections/Mix [boxset]/folder.png",
        true
    )]
    #[case::beside_a_genre(
        "Genre",
        "/metadata/Genre/Crime",
        false,
        Primary,
        "/metadata/Genre/Crime/folder.png",
        true
    )]
    #[case::beside_a_movie(
        "Movie",
        "/media/Heat/Heat.mkv",
        false,
        Primary,
        "/media/Heat/poster.jpg",
        true
    )]
    #[case::in_extrafanart(
        "Movie",
        "/media/Heat/Heat.mkv",
        false,
        Backdrop,
        "/media/Heat/extrafanart/fanart1.jpg",
        true
    )]
    #[case::remote(
        "Movie",
        "/media/Heat/Heat.mkv",
        false,
        Primary,
        "https://image.tmdb.org/t/p/original/a.jpg",
        true
    )]
    #[case::beside_an_episode(
        "Episode",
        "/tv/Show/Season 1/e1.mkv",
        false,
        Primary,
        "/tv/Show/Season 1/e1-thumb.jpg",
        true
    )]
    #[case::another_movies_folder(
        "Movie",
        "/media/Heat/Heat.mkv",
        false,
        Primary,
        "/media/Ronin/poster.jpg",
        false
    )]
    #[case::a_folder_named_alike(
        "Movie",
        "/media/Heat/Heat.mkv",
        false,
        Primary,
        "/media/Heat2/poster.jpg",
        false
    )]
    #[case::the_items_own_file(
        "Movie",
        "/media/Heat/Heat.mkv",
        false,
        Primary,
        "/media/Heat/Heat.mkv",
        false
    )]
    #[case::a_shared_album_cover(
        "MusicAlbum",
        "/media/Album",
        true,
        Primary,
        "/media/Album/album-covers/a/b.jpg",
        false
    )]
    #[case::relative("Movie", "/media/Heat/Heat.mkv", false, Primary, "poster.jpg", false)]
    #[case::a_track(
        "Audio",
        "/media/Album/01.flac",
        false,
        Primary,
        "/media/Album/cover.jpg",
        false
    )]
    #[case::a_photo(
        "Photo",
        "/media/Trip/a.jpg",
        false,
        Primary,
        "/media/Trip/folder.jpg",
        false
    )]
    #[case::an_episode_backdrop(
        "Episode",
        "/tv/Show/Season 1/e1.mkv",
        false,
        Backdrop,
        "/tv/Show/Season 1/e1-fanart.jpg",
        false
    )]
    fn beside_art_is_taken_only_where_the_saver_writes(
        #[case] kind: &str,
        #[case] path: &str,
        #[case] is_folder: bool,
        #[case] image_type: ferrofin_model::entities::ImageType,
        #[case] image_path: &str,
        #[case] removes: bool,
    ) {
        let beside = BesideArt {
            item: row(kind, path, is_folder),
            series: None,
            photos: false,
        };
        assert_eq!(beside.removes(&image(image_type, image_path)), removes);
    }

    /// gl-1741: a season's art is saved in its series' folder
    /// (`seasonNN-poster`), so it is taken from there, and not from another
    /// series.
    #[test]
    fn a_seasons_beside_art_is_its_series_folders() {
        let beside = BesideArt {
            item: BaseItemEntity {
                index_number: Some(1),
                ..row("Season", "/tv/Show/Season 1", true)
            },
            series: Some(row("Series", "/tv/Show", true)),
            photos: false,
        };
        assert!(beside.removes(&image(Primary, "/tv/Show/season01-poster.jpg")));
        assert!(!beside.removes(&image(Primary, "/tv/Other/season01-poster.jpg")));
    }

    /// gl-1741: in a home-video (and photo) library an image beside an album
    /// can be one of its photos, so no local file is taken there; a remote
    /// row still is.
    #[test]
    fn nothing_beside_an_item_in_a_photo_library_is_taken() {
        let beside = BesideArt {
            item: row("PhotoAlbum", "/photos/Trip", true),
            series: None,
            photos: true,
        };
        assert!(!beside.removes(&image(Primary, "/photos/Trip/jacket.jpg")));
        assert!(!beside.removes(&image(Backdrop, "/photos/Trip/backdrop.jpg")));
        assert!(beside.removes(&image(Primary, "https://img/trip.jpg")));
    }

    /// gl-1741, through `remove_images`: with the art saved beside a movie,
    /// its first Primary and its backdrops there go, files and rows, and a
    /// remote image's row; a second Primary, a screenshot, an image in
    /// another folder, the movie and a file it does not list stay. Without
    /// (the library does not save artwork into its media folders) the media
    /// folder is left as it is.
    #[rstest::rstest]
    #[case::saved_beside(true)]
    #[case::not_saved_beside(false)]
    fn removing_the_images_saved_beside_a_movie(#[case] saved_beside: bool) {
        use ferrofin_model::entities::ImageType::{Screenshot, Thumb};
        let tmp = tempfile::tempdir().unwrap();
        let movie = tmp.path().join("media/Heat");
        let other = tmp.path().join("media/Ronin");
        std::fs::create_dir_all(movie.join("extrafanart")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let file = |path: &Path| {
            std::fs::write(path, b"x").unwrap();
            path.to_string_lossy().into_owned()
        };
        let mkv = file(&movie.join("Heat.mkv"));
        let unlisted = file(&movie.join("notes.jpg"));
        let poster = image(Primary, &file(&movie.join("poster.jpg")));
        let second = image(Primary, &file(&movie.join("poster1.jpg")));
        let fanart = image(Backdrop, &file(&movie.join("fanart.jpg")));
        let extra = image(Backdrop, &file(&movie.join("extrafanart/fanart1.jpg")));
        let screenshot = image(Screenshot, &file(&movie.join("screenshot.jpg")));
        let elsewhere = image(Thumb, &file(&other.join("landscape.jpg")));
        let remote = image(
            ferrofin_model::entities::ImageType::Logo,
            "https://img/logo.png",
        );
        let all = [
            &poster,
            &second,
            &fanart,
            &extra,
            &screenshot,
            &elsewhere,
            &remote,
        ];
        let mut images: Vec<ItemImageInfo> = all.iter().map(|i| (*i).clone()).collect();
        let beside = saved_beside.then(|| BesideArt {
            item: row("Movie", &mkv, false),
            series: None,
            photos: false,
        });
        let metadata = [tmp.path().join("metadata/library/x")];

        let removed =
            crate::local_images::remove_images(&mut images, &metadata, "x", beside.as_ref());

        assert_eq!(removed, saved_beside);
        let kept: Vec<&ItemImageInfo> = if saved_beside {
            vec![&second, &screenshot, &elsewhere]
        } else {
            all.to_vec()
        };
        assert_eq!(images.iter().collect::<Vec<_>>(), kept);
        for image in [&poster, &second, &fanart, &extra, &screenshot, &elsewhere] {
            assert_eq!(
                Path::new(&image.path).exists(),
                kept.contains(&image),
                "{}",
                image.path
            );
        }
        assert!(Path::new(&mkv).exists(), "the movie stays");
        assert!(
            Path::new(&unlisted).exists(),
            "a file the movie does not list stays"
        );
    }
}
