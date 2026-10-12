//! A [`ChapterManager`] with no chapters, for contexts built without the
//! database (extension tests). The composition root injects
//! `ferrofin-core`'s `FerrofinChapterManager`.

use async_trait::async_trait;
use ferrofin_model::entities_media::ChapterInfo;
use uuid::Uuid;

use crate::chapters::ChapterManager;
use crate::error::ServiceError;

/// No item has chapters; saving or deleting them is a backend error.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoChapters;

#[async_trait]
impl ChapterManager for NoChapters {
    async fn supports(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
        Ok(false)
    }

    async fn save_chapters(
        &self,
        _item_id: Uuid,
        _chapters: &[ChapterInfo],
    ) -> Result<(), ServiceError> {
        Err(ServiceError::backend("no chapter store attached"))
    }

    async fn get_chapter(
        &self,
        _item_id: Uuid,
        _index: i32,
    ) -> Result<Option<ChapterInfo>, ServiceError> {
        Ok(None)
    }

    async fn get_chapters(&self, _item_id: Uuid) -> Result<Vec<ChapterInfo>, ServiceError> {
        Ok(Vec::new())
    }

    async fn delete_chapter_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        Err(ServiceError::backend("no chapter store attached"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn there_are_no_chapters() {
        let id = Uuid::from_u128(1);
        assert!(!NoChapters.supports(id).await.unwrap());
        assert!(NoChapters.get_chapters(id).await.unwrap().is_empty());
        assert!(NoChapters.get_chapter(id, 0).await.unwrap().is_none());
        assert!(NoChapters.save_chapters(id, &[]).await.is_err());
        assert!(NoChapters.delete_chapter_data(id).await.is_err());
    }
}
