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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

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
    pub async fn save(
        &self,
        item: &BaseItemEntity,
        image: ItemImageInfo,
        index: usize,
        current: &[ItemImageInfo],
    ) -> Result<ItemImageInfo, ServiceError> {
        let Some(settings) = self.settings(item).await? else {
            return Ok(image);
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
                    Ok(saved) => *image = saved,
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            current.push(image.clone());
        }
        failure.map_or(Ok(()), Err)
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
    ) -> Result<ItemImageInfo, ServiceError> {
        if !self.managed(Path::new(&image.path))
            || !policy::eligible(item, settings.series.as_ref(), image.image_type)
        {
            return Ok(image);
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
            return Ok(image);
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
        let copied = tokio::task::spawn_blocking(move || {
            let result = copy_images(&source, &paths, old.as_deref());
            if result.is_err() && !duplicate && source.is_file() {
                remove_previous(old.as_deref(), std::slice::from_ref(&source));
            }
            result
        })
        .await
        .map_err(|error| ServiceError::backend(error.to_string()))?;
        match copied {
            Ok(()) => Ok(ItemImageInfo {
                path: destination.to_string_lossy().into_owned(),
                date_modified: std::fs::metadata(&destination)
                    .ok()
                    .and_then(|info| info.modified().ok())
                    .map_or(image.date_modified, chrono::DateTime::from),
                ..image
            }),
            Err(error) if duplicate => Err(ServiceError::backend(format!(
                "write duplicated artwork {}: {error}",
                destination.display()
            ))),
            Err(error) => {
                tracing::warn!(%error,path=%destination.display(),"media artwork destination failed; retaining internal image");
                Ok(image)
            }
        }
    }
}

fn shared(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str() == crate::provider_manager::SHARED_ALBUM_ARTWORK_DIR)
}

fn copy_images(source: &Path, paths: &[PathBuf], old: Option<&Path>) -> std::io::Result<()> {
    let bytes = std::fs::read(source)?;
    for path in paths {
        if path != source && std::fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
            ferrofin_util::file_helper::atomic_write_media(path, &bytes)?;
        }
    }
    remove_previous(old, paths);
    if !paths.iter().any(|path| path == source) && !shared(source) {
        let _ = std::fs::remove_file(source);
    }
    Ok(())
}

fn remove_previous(old: Option<&Path>, paths: &[PathBuf]) {
    if let Some(old) = old
        && !paths.iter().any(|path| path == old)
        && let Err(error) = std::fs::remove_file(old)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path=%old.display(), "could not remove replaced artwork");
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
        copy_images(&source, std::slice::from_ref(&dest), Some(&old)).unwrap();
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
