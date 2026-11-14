//! What a deleted item leaves on disk besides its media, removed before its
//! rows go: its metadata folders and its extracted data.
//!
//! Upstream (v12.4) removes every item through `LibraryManager.DeleteItem`
//! (`LibraryManager.cs:421-625`) or `DeleteItemsUnsafeFast` (`:361-419`),
//! the items a library scan drops included (`Folder.ValidateChildrenInternal2`
//! calls `DeleteItem` for each child whose file is gone, `Folder.cs:527`,
//! `:603`). Both delete the
//! metadata paths (`GetMetadataPaths`/`GetInternalMetadataPaths`), then each
//! item's extracted data (`IExternalDataManager.DeleteExternalItemFiles`:
//! attachment and subtitle caches, trickplay tiles in both locations, chapter
//! images), then the rows. [`ItemDataCleanup`] is that cleanup, shared by the
//! library manager's delete and the scan's removals so the two cannot drift.
//! The scan also prunes a video whose file changed of what was extracted from
//! the old one ([`ItemDataCleanup::prune_extracted_data`]).

use std::sync::{Arc, OnceLock};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::system::{ExternalDataManager, MediaLocation, ServerApplicationPaths as _};
use uuid::Uuid;

use crate::app_paths::FerrofinServerApplicationPaths;
use crate::virtual_paths::VirtualPathExpander;

/// Removes deleted items' metadata folders and extracted data, and a changed
/// video's extracted data. Every folder removal is best-effort, as upstream's:
/// a folder that cannot be removed is logged and passed over, and the delete
/// goes on.
pub struct ItemDataCleanup {
    /// The server's directories: the internal metadata root, read at each
    /// delete (an admin can move it), whose per-item folders go.
    paths: Arc<FerrofinServerApplicationPaths>,
    /// Expands a stored `%AppDataPath%`/`%MetadataPath%` path before its
    /// media location is worked out.
    virtual_paths: VirtualPathExpander,
    /// `IExternalDataManager`. Late-bound: the managers that build it (the
    /// chapter and segment managers) come after the library manager and the
    /// scanner. Unset, no extracted data is removed.
    external: OnceLock<Arc<dyn ExternalDataManager>>,
}

impl std::fmt::Debug for ItemDataCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ItemDataCleanup").finish_non_exhaustive()
    }
}

impl ItemDataCleanup {
    /// A cleanup over the server's directories, with no extracted-data
    /// cleanup until [`Self::set_external_data`].
    #[must_use]
    pub fn new(paths: Arc<FerrofinServerApplicationPaths>) -> Self {
        Self {
            virtual_paths: VirtualPathExpander::from_paths(Arc::clone(&paths)),
            paths,
            external: OnceLock::new(),
        }
    }

    /// Late-binds the extracted-data cleanup. A second call is ignored.
    pub fn set_external_data(&self, external: Arc<dyn ExternalDataManager>) {
        let _ = self.external.set(external);
    }

    /// Both halves in upstream's order, for a delete that removes no media
    /// file (`DeleteFileLocation = false`, every scan removal): the metadata
    /// folders ([`Self::delete_metadata_folders`]), then the extracted data
    /// ([`Self::delete_extracted_data`]). The caller deletes the rows after.
    pub(crate) async fn delete_item_data(
        &self,
        rows: &[&BaseItemEntity],
        media_kept: &(dyn Fn(&BaseItemEntity) -> bool + Sync),
    ) {
        self.delete_metadata_folders(rows).await;
        self.delete_extracted_data(rows, media_kept).await;
    }

    /// `GetMetadataPaths(item, children)` (`:558-580`, `:714-743`): each
    /// row's internal metadata folder (`{metadata}/library/{id2}/{id}`),
    /// Ferrofin's own `{metadata}/library/{GUID}`, and the lower-cased one
    /// older scans also wrote art to ([`crate::item_folders::item_art_folders`]).
    ///
    /// Upstream's list also names a video's internal extracted data
    /// (trickplay tiles, chapter images, subtitle and attachment caches);
    /// those go with [`Self::delete_extracted_data`], which removes every one
    /// of them.
    pub(crate) async fn delete_metadata_folders(&self, rows: &[&BaseItemEntity]) {
        // Every layout: Ferrofin's `{metadata}/library/{GUID}` (and its older
        // lower-cased spelling) and Jellyfin's `{metadata}/library/{xx}/{idN}`
        // an adopted item has.
        let art_root = std::path::Path::new(&self.paths.internal_metadata_path()).join("library");
        let folders: Vec<std::path::PathBuf> = rows
            .iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok())
            .flat_map(|id| crate::item_folders::item_art_folders(&art_root, id))
            .collect();
        if folders.is_empty() {
            return;
        }
        let removed = tokio::task::spawn_blocking(move || {
            for folder in folders {
                match std::fs::remove_dir_all(&folder) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => tracing::warn!(
                        path = %folder.display(),
                        %err,
                        "could not delete a deleted item's metadata folder"
                    ),
                }
            }
        })
        .await;
        if let Err(err) = removed {
            tracing::error!(%err, "deleting item metadata folders panicked");
        }
    }

    /// `externalDataManager.DeleteExternalItemFiles` for each row
    /// (`:597-603`, `:405-409`): its attachment and subtitle caches, its internal
    /// trickplay folder, its chapter images, and the trickplay folder saved
    /// beside its media.
    ///
    /// `media_kept` names the rows whose trickplay folder beside the media
    /// may be another item's (a video file of its name is still beside it):
    /// that folder stays, as for an item with no path, which
    /// `GetExtractedDataPaths` gives none
    /// (`if (!string.IsNullOrEmpty(item.Path))`). Their other data is keyed
    /// by their own id and goes.
    pub(crate) async fn delete_extracted_data(
        &self,
        rows: &[&BaseItemEntity],
        media_kept: &(dyn Fn(&BaseItemEntity) -> bool + Sync),
    ) {
        let Some(external) = self.external.get() else {
            tracing::debug!(
                rows = rows.len(),
                "no extracted-data manager bound yet; the rows' extracted data stays"
            );
            return;
        };
        for row in rows {
            let Ok(id) = Uuid::parse_str(&row.id) else {
                continue;
            };
            let (path, containing_folder) = if media_kept(row) {
                (String::new(), String::new())
            } else {
                self.media_of(row)
            };
            let media = MediaLocation {
                path: &path,
                containing_folder: &containing_folder,
            };
            if let Err(err) = external.delete_external_item_files(id, media).await {
                tracing::warn!(item_id = %id, %err, "could not delete a deleted item's extracted data");
            }
        }
    }

    /// `ExternalDataManager.DeleteExternalItemDataAsync` for a video whose
    /// file changed (`MetadataService.BeforeSaveInternal`,
    /// `MetadataService.cs:391-395`): what was extracted from the old file,
    /// its folders and its keyframe, media-segment, trickplay and chapter
    /// rows, goes before the refresh extracts from the new one. A segment
    /// provider's own store is its `cleanup`'s to clear (Intro Skipper's
    /// keeps the old analysis, as upstream's plugin does: gl-1696).
    ///
    /// # Errors
    ///
    /// A row deletion's error, as upstream lets it fail the refresh; a
    /// folder that cannot be removed is logged and passed over.
    pub(crate) async fn prune_extracted_data(
        &self,
        row: &BaseItemEntity,
    ) -> Result<(), ServiceError> {
        let Some(external) = self.external.get() else {
            tracing::debug!(
                item_id = row.id,
                "no extracted-data manager bound yet; a changed file's extracted data stays"
            );
            return Ok(());
        };
        let Ok(id) = Uuid::parse_str(&row.id) else {
            return Ok(());
        };
        let (path, containing_folder) = self.media_of(row);
        tracing::debug!(item_id = %id, path, "file changed, pruning extracted data");
        let media = MediaLocation {
            path: &path,
            containing_folder: &containing_folder,
        };
        external.delete_external_item_data(id, media).await
    }

    /// `row`'s media path, expanded, and the folder holding it; both empty
    /// for a row with no path.
    fn media_of(&self, row: &BaseItemEntity) -> (String, String) {
        let path = row
            .path
            .as_deref()
            .map(|p| self.virtual_paths.expand(p))
            .unwrap_or_default();
        let containing_folder = if path.is_empty() {
            String::new()
        } else {
            crate::video_versions::containing_folder_path_at(row, &path)
        };
        (path, containing_folder)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_traits::error::ServiceError;

    use super::*;

    /// Records each `DeleteExternalItemFiles` call's id and media path.
    #[derive(Default)]
    struct RecordingExternalData {
        calls: Mutex<Vec<(Uuid, String, String)>>,
    }

    #[async_trait]
    impl ExternalDataManager for RecordingExternalData {
        async fn delete_external_item_data(
            &self,
            _item_id: Uuid,
            _media: MediaLocation<'_>,
        ) -> Result<(), ServiceError> {
            unimplemented!("a delete only removes the files")
        }

        async fn delete_external_item_files(
            &self,
            item_id: Uuid,
            media: MediaLocation<'_>,
        ) -> Result<(), ServiceError> {
            self.calls.lock().expect("lock").push((
                item_id,
                media.path.to_owned(),
                media.containing_folder.to_owned(),
            ));
            Ok(())
        }
    }

    fn cleanup(root: &std::path::Path) -> ItemDataCleanup {
        ItemDataCleanup::new(crate::app_paths::test_paths(root))
    }

    fn row(id: Uuid, path: Option<&str>) -> BaseItemEntity {
        BaseItemEntity {
            id: guid_to_db(id),
            path: path.map(str::to_owned),
            ..BaseItemEntity::default()
        }
    }

    /// Each row's art and internal metadata folders go, in every spelling,
    /// and a sibling's stay.
    #[tokio::test]
    async fn every_spelling_of_a_rows_metadata_folder_goes() {
        let temp = tempfile::tempdir().expect("temp");
        let cleanup = cleanup(temp.path());
        let art = std::path::Path::new(&cleanup.paths.internal_metadata_path()).join("library");
        let (gone, kept) = (Uuid::new_v4(), Uuid::new_v4());
        let folders = |id: Uuid| crate::item_folders::item_art_folders(&art, id);
        for folder in folders(gone).iter().chain(folders(kept).iter()) {
            std::fs::create_dir_all(folder.join("x")).expect("folder");
        }
        cleanup.delete_metadata_folders(&[&row(gone, None)]).await;
        assert!(
            folders(gone).iter().all(|f| !f.exists()),
            "the row's folders go"
        );
        assert!(folders(kept).iter().all(|f| f.exists()), "a sibling's stay");
    }

    /// A row whose media another item still uses keeps the trickplay folder
    /// beside it: it is asked about with no path, like an item that has none.
    #[tokio::test]
    async fn a_row_whose_media_is_kept_is_cleaned_without_its_path() {
        let temp = tempfile::tempdir().expect("temp");
        let cleanup = cleanup(temp.path());
        let external = Arc::new(RecordingExternalData::default());
        cleanup.set_external_data(external.clone());
        let (gone, shared) = (Uuid::new_v4(), Uuid::new_v4());
        let (gone_row, shared_row) = (
            row(gone, Some("/media/Gone.mkv")),
            row(shared, Some("/media/Shared.mkv")),
        );
        cleanup
            .delete_extracted_data(&[&gone_row, &shared_row], &|row: &BaseItemEntity| {
                row.id == shared_row.id
            })
            .await;
        assert_eq!(
            *external.calls.lock().expect("lock"),
            vec![
                (gone, "/media/Gone.mkv".to_owned(), "/media".to_owned()),
                (shared, String::new(), String::new()),
            ]
        );
    }

    /// No extracted-data manager bound: nothing is asked, nothing fails.
    #[tokio::test]
    async fn without_an_external_data_manager_nothing_is_removed() {
        let temp = tempfile::tempdir().expect("temp");
        let cleanup = cleanup(temp.path());
        cleanup
            .delete_item_data(&[&row(Uuid::new_v4(), Some("/media/A.mkv"))], &|_| false)
            .await;
    }
}
