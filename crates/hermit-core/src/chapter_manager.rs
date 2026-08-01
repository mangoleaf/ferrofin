//! [`HermitChapterManager`] — a **minimal** [`ChapterManager`] over the
//! unit-2 chapter repository.
//!
//! Port of `Emby.Server.Implementations.Chapters.ChapterManager`. This is a thin
//! unit-8 manager: it delegates row access to the injected
//! [`ChapterRepository`](hermit_traits::persistence::ChapterRepository) and
//! resolves an item's kind through the injected
//! [`LibraryManager`](hermit_traits::library::LibraryManager) to answer
//! [`ChapterManager::supports`]. It converts between the persistence
//! [`ChapterEntity`](hermit_db::entities::base_items::ChapterEntity) and the wire
//! [`ChapterInfo`] the trait speaks.
//!
//! Deferred (documented, per the unit-8 minimal-manager rule): chapter *image*
//! extraction/refresh (`RefreshChapterImages`) needs the un-ported `Video`
//! domain object plus an `IImageProcessor`/directory service and lands in a
//! later wave; the [`ChapterInfo::image_tag`] a DTO would carry is therefore not
//! computed here (left `None`).

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::base_items::ChapterEntity;
use hermit_model::entities_media::ChapterInfo;
use uuid::Uuid;

use hermit_traits::chapters::ChapterManager;
use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryManager;
use hermit_traits::persistence::ChapterRepository;

use crate::item_type_lookup::kind_from_type_name;
use crate::kinds::is_video;

/// The concrete (minimal) chapter manager.
#[derive(Clone)]
pub struct HermitChapterManager {
    repository: Arc<dyn ChapterRepository>,
    library_manager: Arc<dyn LibraryManager>,
}

impl std::fmt::Debug for HermitChapterManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitChapterManager")
            .finish_non_exhaustive()
    }
}

impl HermitChapterManager {
    /// Creates a chapter manager from its injected collaborators.
    #[must_use]
    pub fn new(
        repository: Arc<dyn ChapterRepository>,
        library_manager: Arc<dyn LibraryManager>,
    ) -> Self {
        Self {
            repository,
            library_manager,
        }
    }

    /// Maps a wire [`ChapterInfo`] onto a persistence [`ChapterEntity`] for the
    /// given item; the `item_id`/`chapter_index` are set by the repository on
    /// save, so placeholders are fine here.
    fn to_entity(item_id: Uuid, info: &ChapterInfo) -> ChapterEntity {
        ChapterEntity {
            item_id: item_id.to_string(),
            chapter_index: 0,
            image_date_modified: Some(info.image_date_modified),
            image_path: info.image_path.clone(),
            name: info.name.clone(),
            start_position_ticks: info.start_position_ticks,
        }
    }

    /// Maps a persistence [`ChapterEntity`] onto the wire [`ChapterInfo`]. The
    /// `image_tag` (a DTO-layer image cache tag) is a documented deferral.
    fn to_info(entity: ChapterEntity) -> ChapterInfo {
        ChapterInfo {
            start_position_ticks: entity.start_position_ticks,
            name: entity.name,
            image_path: entity.image_path,
            image_date_modified: entity.image_date_modified.unwrap_or_default(),
            image_tag: None,
        }
    }
}

#[async_trait]
impl ChapterManager for HermitChapterManager {
    async fn supports(&self, item_id: Uuid) -> Result<bool, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(item_id).await? else {
            return Ok(false);
        };
        // Chapters are a `Video` behavior in C# (`item is Video`).
        Ok(kind_from_type_name(&item.type_).is_some_and(is_video))
    }

    async fn save_chapters(
        &self,
        item_id: Uuid,
        chapters: &[ChapterInfo],
    ) -> Result<(), ServiceError> {
        let entities: Vec<ChapterEntity> = chapters
            .iter()
            .map(|c| Self::to_entity(item_id, c))
            .collect();
        self.repository.save_chapters(item_id, &entities).await
    }

    async fn get_chapter(
        &self,
        item_id: Uuid,
        index: i32,
    ) -> Result<Option<ChapterInfo>, ServiceError> {
        Ok(self
            .repository
            .get_chapter(item_id, index)
            .await?
            .map(Self::to_info))
    }

    async fn get_chapters(&self, item_id: Uuid) -> Result<Vec<ChapterInfo>, ServiceError> {
        Ok(self
            .repository
            .get_chapters(item_id)
            .await?
            .into_iter()
            .map(Self::to_info)
            .collect())
    }

    async fn get_chapters_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<ChapterInfo>>, ServiceError> {
        let rows = self.repository.get_chapters_batch(item_ids).await?;
        Ok(rows
            .into_iter()
            .map(|(id, chapters)| (id, chapters.into_iter().map(Self::to_info).collect()))
            .collect())
    }

    async fn delete_chapter_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
        self.repository.delete_chapters(item_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hermit_model::data::BaseItemKind;
    use hermit_model::entities_media::ChapterInfo;
    use uuid::Uuid;

    use hermit_traits::chapters::ChapterManager;

    use crate::chapter_repository::HermitChapterRepository;
    use crate::test_support::{library_manager_over, seed_item, test_db};

    use super::HermitChapterManager;

    fn chapter(name: &str, ticks: i64) -> ChapterInfo {
        ChapterInfo {
            start_position_ticks: ticks,
            name: Some(name.to_owned()),
            ..ChapterInfo::default()
        }
    }

    #[tokio::test]
    async fn save_get_and_delete_round_trip() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;

        let repo = Arc::new(HermitChapterRepository::new(db.clone()));
        let library = library_manager_over(db.clone());
        let mgr = HermitChapterManager::new(repo, library);

        mgr.save_chapters(item, &[chapter("Intro", 0), chapter("Outro", 100)])
            .await
            .expect("save");

        let all = mgr.get_chapters(item).await.expect("get");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name.as_deref(), Some("Intro"));

        let one = mgr.get_chapter(item, 1).await.expect("one");
        assert_eq!(one.expect("present").name.as_deref(), Some("Outro"));

        mgr.delete_chapter_data(item).await.expect("delete");
        assert!(mgr.get_chapters(item).await.expect("get").is_empty());
    }

    #[tokio::test]
    async fn supports_only_video_kinds() {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        let series = Uuid::new_v4();
        seed_item(&db, movie, BaseItemKind::Movie).await;
        seed_item(&db, series, BaseItemKind::Series).await;

        let repo = Arc::new(HermitChapterRepository::new(db.clone()));
        let mgr = HermitChapterManager::new(repo, library_manager_over(db.clone()));

        assert!(mgr.supports(movie).await.expect("movie"));
        assert!(!mgr.supports(series).await.expect("series"));
        assert!(!mgr.supports(Uuid::new_v4()).await.expect("missing"));
    }
}
