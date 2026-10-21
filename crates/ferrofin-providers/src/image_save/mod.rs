//! The shared media-adjacent image writer used by scans and manual uploads.
//! Names, eligibility and read-only fallback follow ImageSaver at 4910aafa1a.
mod policy;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{ImageSavingConvention, XbmcMetadataOptions};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_util::directory_path::DirectoryPath;
use ferrofin_util::string_extensions::replace_ignore_ascii_case;
use std::borrow::Cow;
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

/// Publishes acquired artwork beside media when the owning library requests it.
/// Inputs are complete staged images; an unwritable local destination retains
/// that internal image as the ordinary ImageSaver fallback.
pub struct ImageFileSaver {
    items: Arc<dyn ItemRepository>,
    folders: Arc<dyn VirtualFolderManager>,
    configuration: Arc<dyn ServerConfigurationManager>,
    metadata: DirectoryPath,
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
        }
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
        let item = &self.expand_path(item);
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
        let item = &self.expand_path(item);
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
            if !previous.contains(image) && self.managed(Path::new(&image.path)) {
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

    /// `ExpandVirtualPath` on the item's own path (`BaseItemRepository.Map`):
    /// an adopted genre or playlist stores `%MetadataPath%/Genre/…` or
    /// `%AppDataPath%/playlists/…`, and the item read leaves it as stored.
    fn expand_path<'a>(&self, item: &'a BaseItemEntity) -> Cow<'a, BaseItemEntity> {
        let Some(path) = item.path.as_deref().filter(|path| path.contains('%')) else {
            return Cow::Borrowed(item);
        };
        let paths = self.configuration.application_paths();
        let path = replace_ignore_ascii_case(path, "%AppDataPath%", &paths.data_path());
        let path =
            replace_ignore_ascii_case(&path, "%MetadataPath%", &paths.internal_metadata_path());
        Cow::Owned(BaseItemEntity {
            path: Some(path),
            ..item.clone()
        })
    }

    /// Whether the item's own path lies in one of the server's own
    /// directories rather than in a media library: the data directory (a
    /// playlist in `{data}/playlists`, a box set in `{data}/collections`), the
    /// internal metadata directory (a genre's `{metadata}/Genre/<name>`, a
    /// person's `{metadata}/People/…`) or the libraries' root folder (a
    /// library's own `root/default/<name>`). The check is lexical, so a path
    /// it cannot match falls back to the library's setting.
    fn in_server_directories(&self, item: &BaseItemEntity) -> bool {
        let Some(path) = item.path.as_deref().map(Path::new) else {
            return false;
        };
        let paths = self.configuration.application_paths();
        [
            paths.data_path(),
            paths.internal_metadata_path(),
            paths.root_folder_path(),
        ]
        .iter()
        .any(|root| !root.is_empty() && path.starts_with(root))
    }

    async fn settings(&self, item: &BaseItemEntity) -> Result<Option<Settings>, ServiceError> {
        let series = if policy::short_kind(item) == "Season" {
            let id = item
                .series_id
                .as_deref()
                .or(item.parent_id.as_deref())
                .and_then(|id| Uuid::parse_str(id).ok());
            if let Some(id) = id {
                self.items.retrieve_item(id).await?
            } else {
                None
            }
        } else {
            None
        };
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
        if !(matches!(
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
        ) && self.in_server_directories(item))
        {
            let folders = self.folders.get_virtual_folders().await?;
            let options = ferrofin_model::entities_media::owning_library(
                &folders,
                item.top_parent_id.as_deref(),
                item.path
                    .as_deref()
                    .or_else(|| series.as_ref().and_then(|series| series.path.as_deref())),
            )
            .and_then(|folder| folder.library_options.as_ref());
            if !options.is_some_and(|options| options.save_local_metadata) {
                return Ok(None);
            }
        }
        let configuration = self.configuration.configuration().await?;
        let path = PathBuf::from(
            self.configuration
                .application_paths()
                .user_configuration_directory_path(),
        )
        .join("named/xbmcmetadata.json");
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
        let (copied, cleanup) = tokio::task::spawn_blocking(move || {
            match copy_images(&source, &paths, old.as_deref()) {
                Ok(cleanup) => (Ok(()), cleanup),
                Err(error) => {
                    // The retained internal image is the new image, so the
                    // replaced file still goes.
                    let cleanup = (!duplicate && source.is_file())
                        .then(|| remove_previous(old.as_deref(), std::slice::from_ref(&source)))
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
                old_path.as_deref().unwrap_or(Path::new("")).display()
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
fn copy_images(
    source: &Path,
    paths: &[PathBuf],
    old: Option<&Path>,
) -> std::io::Result<Option<std::io::Error>> {
    let bytes = std::fs::read(source)?;
    for path in paths {
        if path != source && std::fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
            ferrofin_util::file_helper::atomic_write_media(path, &bytes)?;
        }
    }
    let cleanup = remove_previous(old, paths).err();
    if !paths.iter().any(|path| path == source) && !shared(source) {
        let _ = std::fs::remove_file(source);
    }
    Ok(cleanup)
}

/// `ImageSaver`'s "Delete the current path": skipped when any saved path equals
/// it ignoring case (`OrdinalIgnoreCase`), and only "file not found" is
/// forgiven — every other failure is returned.
fn remove_previous(old: Option<&Path>, paths: &[PathBuf]) -> std::io::Result<()> {
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
    match std::fs::remove_file(old) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publishing_replaces_old_extension_and_preserves_identical_destination_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("internal.png");
        let old = tmp.path().join("folder.jpg");
        let dest = tmp.path().join("folder.png");
        std::fs::write(&source, b"art").unwrap();
        std::fs::write(&old, b"old").unwrap();
        assert!(
            copy_images(&source, std::slice::from_ref(&dest), Some(&old))
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"art");
        assert!(!source.exists());
        assert!(!old.exists());
        let modified = std::fs::metadata(&dest).unwrap().modified().unwrap();
        std::fs::write(&source, b"art").unwrap();
        copy_images(&source, std::slice::from_ref(&dest), None).unwrap();
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
        let cleanup = copy_images(&source, std::slice::from_ref(&dest), Some(&stuck)).unwrap();
        assert!(cleanup.is_some());
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!source.exists());
        // An already-absent previous file is fine.
        std::fs::write(&source, b"new").unwrap();
        let missing = tmp.path().join("gone.jpg");
        assert!(
            copy_images(&source, std::slice::from_ref(&dest), Some(&missing))
                .unwrap()
                .is_none()
        );
        // A previous path equal to a saved path ignoring case is kept.
        std::fs::write(&source, b"new").unwrap();
        let alias = tmp.path().join("FOLDER.PNG");
        std::fs::write(&alias, b"old").unwrap();
        assert!(
            copy_images(&source, std::slice::from_ref(&dest), Some(&alias))
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
        assert!(copy_images(&source, std::slice::from_ref(&blocked), Some(&old)).is_err());
        assert!(source.exists());
        assert!(old.exists());
        let shared = tmp.path().join("album-covers/album/content.png");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(&shared, b"shared").unwrap();
        let outputs = [
            tmp.path().join("extrafanart/fanart1.png"),
            tmp.path().join("extrathumbs/thumb1.png"),
        ];
        copy_images(&shared, &outputs, None).unwrap();
        assert!(shared.exists());
        for output in outputs {
            assert_eq!(std::fs::read(output).unwrap(), b"shared");
        }
    }
}
