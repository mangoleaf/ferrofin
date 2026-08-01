//! Chapter manager trait — per-item chapter markers.
//!
//! Port of `MediaBrowser.Controller.Chapters.IChapterManager`.
//!
//! Port rules applied:
//! - The C# `BaseItem` / `Video` receivers become [`uuid::Uuid`] identity
//!   arguments; the `Supports` predicate takes an id.
//! - Chapter data crosses the API boundary as the [`ChapterInfo`] wire DTO (the
//!   C# `ChapterInfo` is a `MediaBrowser.Model.Entities` value type, not an EF
//!   row). The `hermit-db`
//!   [`ChapterEntity`](hermit_db::entities::base_items::ChapterEntity) stays
//!   inside the impl.
//! - `SaveChapters`/`GetChapters` are synchronous in C# but stay `async fn ->
//!   Result` here so the impl may hit the database and surface failures
//!   uniformly.
//! - `RefreshChapterImages` depends on the un-ported domain `Video` plus a
//!   directory service; it is dropped here and resurfaces as a `hermit-core`
//!   free function in Wave 6.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` is
//!   dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;
use hermit_model::entities_media::ChapterInfo;
use uuid::Uuid;

use crate::error::ServiceError;

/// Stores and retrieves the chapter markers attached to library items.
///
/// Port of `IChapterManager`.
#[async_trait]
pub trait ChapterManager: Send + Sync {
    /// Whether the item's type supports chapter markers.
    async fn supports(&self, item_id: Uuid) -> Result<bool, ServiceError>;

    /// Replaces the full set of chapters stored for an item.
    async fn save_chapters(
        &self,
        item_id: Uuid,
        chapters: &[ChapterInfo],
    ) -> Result<(), ServiceError>;

    /// Gets a single chapter of an item by its zero-based index.
    async fn get_chapter(
        &self,
        item_id: Uuid,
        index: i32,
    ) -> Result<Option<ChapterInfo>, ServiceError>;

    /// Gets all chapters for an item, ordered by position.
    async fn get_chapters(&self, item_id: Uuid) -> Result<Vec<ChapterInfo>, ServiceError>;

    /// Batch form of [`Self::get_chapters`] for a page, keyed by item. The default
    /// loops the single-item form; the concrete manager overrides it.
    async fn get_chapters_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<ChapterInfo>>, ServiceError> {
        let mut map = std::collections::HashMap::with_capacity(item_ids.len());
        for &id in item_ids {
            map.insert(id, self.get_chapters(id).await?);
        }
        Ok(map)
    }

    /// Deletes all chapter data for an item.
    async fn delete_chapter_data(&self, item_id: Uuid) -> Result<(), ServiceError>;
}

fn _assert_object_safe_chapter_manager(_: &dyn ChapterManager) {}
