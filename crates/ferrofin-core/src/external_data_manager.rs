//! [`FerrofinExternalDataManager`] — the concrete [`ExternalDataManager`].
//!
//! Port of `Emby.Server.Implementations.Library.ExternalDataManager`. Deletes an
//! item's derived data on two sides:
//! - the *filesystem* side — the attachment/subtitle/trickplay/chapter folders
//!   reported by the [`PathManager`]; and
//! - the *database* side — the keyframe, media-segment, trickplay, and chapter
//!   rows, delegated to the four sibling managers.
//!
//! Sibling managers are taken as `Arc<dyn Trait>` (dependency injection at the
//! Wave 8 composition root); this crate does not construct them. The C# names
//! map to the ferrofin-traits members as: `IKeyframeManager.DeleteKeyframeDataAsync`
//! → [`KeyframeRepository::delete_keyframe_data`],
//! `IMediaSegmentManager.DeleteSegmentsAsync` →
//! [`MediaSegmentManager::delete_segments`],
//! `ITrickplayManager.DeleteTrickplayDataAsync` →
//! [`TrickplayManager::delete_trickplay_data`], and
//! `IChapterManager.DeleteChapterDataAsync` →
//! [`ChapterManager::delete_chapter_data`].
//!
//! Filesystem removal goes through a small [`DirectoryRemover`] seam so tests can
//! run against temp dirs (or a recording fake) instead of touching real storage.
//! As in C#, a per-folder removal failure is logged and skipped, not propagated —
//! pruning is best-effort.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_traits::chapters::ChapterManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::persistence::KeyframeRepository;
use ferrofin_traits::system::{ExternalDataManager, PathManager};
use ferrofin_traits::trickplay::TrickplayManager;
use uuid::Uuid;

/// Removes a directory tree from storage.
///
/// A one-method seam over `std::fs::remove_dir_all` so the external-data
/// manager can be tested without a real filesystem. The default
/// [`FsDirectoryRemover`] performs the real removal.
pub trait DirectoryRemover: Send + Sync {
    /// Whether the directory exists (skip removal when it does not).
    fn exists(&self, path: &str) -> bool;

    /// Recursively removes the directory at `path`.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error; the caller logs and swallows it
    /// (removal is best-effort).
    fn remove_dir_all(&self, path: &str) -> std::io::Result<()>;
}

/// The real, `std::fs`-backed [`DirectoryRemover`].
#[derive(Debug, Clone, Copy, Default)]
pub struct FsDirectoryRemover;

impl DirectoryRemover for FsDirectoryRemover {
    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
    }

    fn remove_dir_all(&self, path: &str) -> std::io::Result<()> {
        std::fs::remove_dir_all(path)
    }
}

/// The concrete external-data manager.
pub struct FerrofinExternalDataManager {
    path_manager: Arc<dyn PathManager>,
    keyframe_repository: Arc<dyn KeyframeRepository>,
    media_segment_manager: Arc<dyn MediaSegmentManager>,
    trickplay_manager: Arc<dyn TrickplayManager>,
    chapter_manager: Arc<dyn ChapterManager>,
    directory_remover: Arc<dyn DirectoryRemover>,
}

impl std::fmt::Debug for FerrofinExternalDataManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinExternalDataManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinExternalDataManager {
    /// Creates an external-data manager over its injected collaborators, using
    /// the real filesystem for directory removal.
    #[must_use]
    pub fn new(
        path_manager: Arc<dyn PathManager>,
        keyframe_repository: Arc<dyn KeyframeRepository>,
        media_segment_manager: Arc<dyn MediaSegmentManager>,
        trickplay_manager: Arc<dyn TrickplayManager>,
        chapter_manager: Arc<dyn ChapterManager>,
    ) -> Self {
        Self::with_remover(
            path_manager,
            keyframe_repository,
            media_segment_manager,
            trickplay_manager,
            chapter_manager,
            Arc::new(FsDirectoryRemover),
        )
    }

    /// Creates an external-data manager with an explicit [`DirectoryRemover`]
    /// (tests inject a fake or a temp-dir-backed remover).
    #[must_use]
    pub fn with_remover(
        path_manager: Arc<dyn PathManager>,
        keyframe_repository: Arc<dyn KeyframeRepository>,
        media_segment_manager: Arc<dyn MediaSegmentManager>,
        trickplay_manager: Arc<dyn TrickplayManager>,
        chapter_manager: Arc<dyn ChapterManager>,
        directory_remover: Arc<dyn DirectoryRemover>,
    ) -> Self {
        Self {
            path_manager,
            keyframe_repository,
            media_segment_manager,
            trickplay_manager,
            chapter_manager,
            directory_remover,
        }
    }

    /// Removes every extracted-data folder for an item, logging and skipping
    /// failures (C# `DeleteExternalItemFiles`).
    fn delete_files(&self, item_id: Uuid, media_path: &str) {
        for path in self.path_manager.extracted_data_paths(item_id, media_path) {
            if !self.directory_remover.exists(&path) {
                continue;
            }
            if let Err(err) = self.directory_remover.remove_dir_all(&path) {
                tracing::warn!(%path, %err, "unable to prune external item data");
            }
        }
    }
}

#[async_trait]
impl ExternalDataManager for FerrofinExternalDataManager {
    async fn delete_external_item_data(
        &self,
        item_id: Uuid,
        media_path: &str,
    ) -> Result<(), ServiceError> {
        // Filesystem first, then the four DB-side deletions (C# ordering).
        self.delete_files(item_id, media_path);
        self.keyframe_repository
            .delete_keyframe_data(item_id)
            .await?;
        self.media_segment_manager.delete_segments(item_id).await?;
        self.trickplay_manager
            .delete_trickplay_data(item_id)
            .await?;
        self.chapter_manager.delete_chapter_data(item_id).await?;
        Ok(())
    }

    async fn delete_external_item_files(
        &self,
        item_id: Uuid,
        media_path: &str,
    ) -> Result<(), ServiceError> {
        self.delete_files(item_id, media_path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use ferrofin_db::entities::base_items::KeyframeDataEntity;
    use ferrofin_db::entities::playback::TrickplayInfoEntity;
    use ferrofin_model::entities_media::ChapterInfo;
    use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
    use ferrofin_traits::media_segments::MediaSegmentProviderInfo;

    // --- Fakes for the injected collaborators ---

    #[derive(Default)]
    struct RecordingRemover {
        removed: Mutex<Vec<String>>,
        existing: Vec<String>,
    }

    impl DirectoryRemover for RecordingRemover {
        fn exists(&self, path: &str) -> bool {
            self.existing.iter().any(|p| p == path)
        }
        fn remove_dir_all(&self, path: &str) -> std::io::Result<()> {
            self.removed.lock().unwrap().push(path.to_owned());
            Ok(())
        }
    }

    struct StubPathManager {
        paths: Vec<String>,
    }
    impl PathManager for StubPathManager {
        fn trickplay_directory(&self, _: Uuid, _: &str, _: bool) -> String {
            String::new()
        }
        fn subtitle_path(&self, _: &str, _: i32, _: &str) -> Option<String> {
            None
        }
        fn subtitle_folder_path(&self, _: &str) -> Option<String> {
            None
        }
        fn attachment_path(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn attachment_folder_path(&self, _: &str) -> Option<String> {
            None
        }
        fn chapter_image_folder_path(&self, _: Uuid, _: &str) -> String {
            String::new()
        }
        fn chapter_image_path(&self, _: Uuid, _: &str, _: i64) -> String {
            String::new()
        }
        fn extracted_data_paths(&self, _: Uuid, _: &str) -> Vec<String> {
            self.paths.clone()
        }
    }

    /// Records every DB-side deletion so the port's delegation can be asserted.
    /// Non-delete methods are unreachable in these tests and simply return empty
    /// results.
    #[derive(Default)]
    struct RecordingDeletes {
        keyframe: Mutex<Vec<Uuid>>,
        segments: Mutex<Vec<Uuid>>,
        trickplay: Mutex<Vec<Uuid>>,
        chapters: Mutex<Vec<Uuid>>,
    }

    #[async_trait]
    impl KeyframeRepository for RecordingDeletes {
        async fn get_keyframe_data(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<KeyframeDataEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn save_keyframe_data(
            &self,
            _item_id: Uuid,
            _data: &KeyframeDataEntity,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_keyframe_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
            self.keyframe.lock().unwrap().push(item_id);
            Ok(())
        }
    }

    #[async_trait]
    impl MediaSegmentManager for RecordingDeletes {
        async fn is_type_supported(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn create_segment(
            &self,
            segment: &MediaSegmentDto,
            _segment_provider_id: &str,
        ) -> Result<MediaSegmentDto, ServiceError> {
            Ok(segment.clone())
        }
        async fn delete_segment(&self, _segment_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_segments(&self, item_id: Uuid) -> Result<(), ServiceError> {
            self.segments.lock().unwrap().push(item_id);
            Ok(())
        }
        async fn delete_provider_segments(
            &self,
            _item_id: Uuid,
            _provider_id: &str,
            _type_filter: Option<MediaSegmentType>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_segments(
            &self,
            _item_id: Uuid,
            _type_filter: Option<&[MediaSegmentType]>,
            _filter_by_provider: bool,
        ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
            Ok(vec![])
        }
        async fn has_segments(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(false)
        }
        async fn get_supported_providers(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError> {
            Ok(vec![])
        }
    }

    #[async_trait]
    impl TrickplayManager for RecordingDeletes {
        async fn refresh_trickplay_data(
            &self,
            _item_id: Uuid,
            _replace: bool,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_trickplay_resolutions(
            &self,
            _item_id: Uuid,
        ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError> {
            Ok(HashMap::new())
        }
        async fn get_trickplay_items(
            &self,
            _limit: i32,
            _offset: i32,
        ) -> Result<Vec<TrickplayInfoEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn save_trickplay_info(
            &self,
            _info: &TrickplayInfoEntity,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_trickplay_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
            self.trickplay.lock().unwrap().push(item_id);
            Ok(())
        }
        async fn get_trickplay_manifest(
            &self,
            _item_id: Uuid,
        ) -> Result<HashMap<String, HashMap<i32, TrickplayInfoEntity>>, ServiceError> {
            Ok(HashMap::new())
        }
        async fn get_hls_playlist(
            &self,
            _item_id: Uuid,
            _width: i32,
            _api_key: Option<&str>,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn get_trickplay_tile_path(
            &self,
            _item_id: Uuid,
            _width: i32,
            _index: i32,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
    }

    #[async_trait]
    impl ChapterManager for RecordingDeletes {
        async fn supports(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn save_chapters(
            &self,
            _item_id: Uuid,
            _chapters: &[ChapterInfo],
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_chapter(
            &self,
            _item_id: Uuid,
            _index: i32,
        ) -> Result<Option<ChapterInfo>, ServiceError> {
            Ok(None)
        }
        async fn get_chapters(&self, _item_id: Uuid) -> Result<Vec<ChapterInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn delete_chapter_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
            self.chapters.lock().unwrap().push(item_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn deletes_existing_folders_and_all_db_data() {
        let item = Uuid::new_v4();
        let remover = Arc::new(RecordingRemover {
            existing: vec!["/x/a".to_owned(), "/x/c".to_owned()],
            ..Default::default()
        });
        let path_manager = Arc::new(StubPathManager {
            paths: vec!["/x/a".to_owned(), "/x/b".to_owned(), "/x/c".to_owned()],
        });
        let deletes = Arc::new(RecordingDeletes::default());

        let mgr = FerrofinExternalDataManager::with_remover(
            path_manager,
            Arc::clone(&deletes) as Arc<dyn KeyframeRepository>,
            Arc::clone(&deletes) as Arc<dyn MediaSegmentManager>,
            Arc::clone(&deletes) as Arc<dyn TrickplayManager>,
            Arc::clone(&deletes) as Arc<dyn ChapterManager>,
            Arc::clone(&remover) as Arc<dyn DirectoryRemover>,
        );

        mgr.delete_external_item_data(item, "/media/x.mkv")
            .await
            .expect("delete");

        // Only the existing folders were removed (/x/b skipped).
        assert_eq!(*remover.removed.lock().unwrap(), vec!["/x/a", "/x/c"]);
        // Every DB-side deletion fired once for the item.
        assert_eq!(*deletes.keyframe.lock().unwrap(), vec![item]);
        assert_eq!(*deletes.segments.lock().unwrap(), vec![item]);
        assert_eq!(*deletes.trickplay.lock().unwrap(), vec![item]);
        assert_eq!(*deletes.chapters.lock().unwrap(), vec![item]);
    }

    #[tokio::test]
    async fn files_only_leaves_db_untouched() {
        let item = Uuid::new_v4();
        let remover = Arc::new(RecordingRemover {
            existing: vec!["/x/a".to_owned()],
            ..Default::default()
        });
        let path_manager = Arc::new(StubPathManager {
            paths: vec!["/x/a".to_owned()],
        });
        let deletes = Arc::new(RecordingDeletes::default());

        let mgr = FerrofinExternalDataManager::with_remover(
            path_manager,
            Arc::clone(&deletes) as Arc<dyn KeyframeRepository>,
            Arc::clone(&deletes) as Arc<dyn MediaSegmentManager>,
            Arc::clone(&deletes) as Arc<dyn TrickplayManager>,
            Arc::clone(&deletes) as Arc<dyn ChapterManager>,
            Arc::clone(&remover) as Arc<dyn DirectoryRemover>,
        );

        mgr.delete_external_item_files(item, "/media/x.mkv")
            .await
            .expect("delete files");

        assert_eq!(*remover.removed.lock().unwrap(), vec!["/x/a"]);
        assert!(deletes.keyframe.lock().unwrap().is_empty());
    }
}
