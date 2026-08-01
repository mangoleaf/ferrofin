//! [`HermitChapterRepository`] — the concrete [`ChapterRepository`] over
//! `hermit-db`.
//!
//! Port of `ChapterRepository`. Reads and writes the `Chapters` table. In C# the
//! repository maps between the persisted `Chapter` entity and the domain
//! `ChapterInfo` (computing an image cache tag via `IImageProcessor`); the trait
//! here works directly on [`ChapterEntity`] rows (per the persistence-trait port
//! rules), so the `IImageProcessor` dependency — a pure DTO-layer concern — is
//! not needed and the save/get are plain row operations.
//!
//! `SaveChapters` in C# deletes then re-inserts inside a transaction; that is
//! preserved here so a save fully replaces the item's chapter set. The
//! zero-based [`ChapterEntity::chapter_index`] is taken from the caller-supplied
//! slice position, matching the C# `for (var i = 0; …)` indexing.

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::base_items::ChapterEntity;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::persistence::ChapterRepository;

use crate::db_error::db_err;

/// The concrete chapter repository.
#[derive(Clone)]
pub struct HermitChapterRepository {
    db: Database,
}

impl std::fmt::Debug for HermitChapterRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitChapterRepository")
            .finish_non_exhaustive()
    }
}

impl HermitChapterRepository {
    /// Creates a chapter repository over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ChapterRepository for HermitChapterRepository {
    async fn delete_chapters(&self, item_id: Uuid) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "Chapters" WHERE "ItemId" = ?1"#)
            .bind(item_id.to_string())
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn save_chapters(
        &self,
        item_id: Uuid,
        chapters: &[ChapterEntity],
    ) -> Result<(), ServiceError> {
        let mut tx = self.db.pool().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "Chapters" WHERE "ItemId" = ?1"#)
            .bind(item_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for (index, chapter) in chapters.iter().enumerate() {
            let chapter_index = i64::try_from(index).unwrap_or(i64::MAX);
            sqlx::query(
                r#"INSERT INTO "Chapters"
                   ("ItemId", "ChapterIndex", "ImageDateModified", "ImagePath",
                    "Name", "StartPositionTicks")
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
            )
            .bind(item_id.to_string())
            .bind(chapter_index)
            .bind(chapter.image_date_modified)
            .bind(&chapter.image_path)
            .bind(&chapter.name)
            .bind(chapter.start_position_ticks)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn get_chapters(&self, item_id: Uuid) -> Result<Vec<ChapterEntity>, ServiceError> {
        let rows = sqlx::query_as::<_, ChapterEntity>(
            r#"SELECT * FROM "Chapters" WHERE "ItemId" = ?1 ORDER BY "StartPositionTicks""#,
        )
        .bind(item_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn get_chapters_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<ChapterEntity>>, ServiceError> {
        let mut map: std::collections::HashMap<Uuid, Vec<ChapterEntity>> =
            std::collections::HashMap::with_capacity(item_ids.len());
        if item_ids.is_empty() {
            return Ok(map);
        }
        for chunk in item_ids.chunks(500) {
            let ph = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT * FROM "Chapters" WHERE "ItemId" IN ({ph})
                   ORDER BY "ItemId", "StartPositionTicks""#,
            );
            let mut query = sqlx::query_as::<_, ChapterEntity>(&sql);
            for id in chunk {
                query = query.bind(id.to_string());
            }
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(id) = Uuid::parse_str(&row.item_id) {
                    map.entry(id).or_default().push(row);
                }
            }
        }
        Ok(map)
    }

    async fn get_chapter(
        &self,
        item_id: Uuid,
        index: i32,
    ) -> Result<Option<ChapterEntity>, ServiceError> {
        let row = sqlx::query_as::<_, ChapterEntity>(
            r#"SELECT * FROM "Chapters" WHERE "ItemId" = ?1 AND "ChapterIndex" = ?2"#,
        )
        .bind(item_id.to_string())
        .bind(i64::from(index))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::HermitChapterRepository;
    use crate::test_support::{seed_item, test_db};
    use hermit_db::entities::base_items::ChapterEntity;
    use hermit_model::data::BaseItemKind;
    use hermit_traits::persistence::ChapterRepository;
    use uuid::Uuid;

    fn chapter(name: &str, ticks: i64) -> ChapterEntity {
        ChapterEntity {
            item_id: String::new(),
            chapter_index: 0,
            image_date_modified: None,
            image_path: None,
            name: Some(name.to_owned()),
            start_position_ticks: ticks,
        }
    }

    #[tokio::test]
    async fn save_replaces_and_indexes_in_order() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = HermitChapterRepository::new(db);

        repo.save_chapters(item, &[chapter("Intro", 0), chapter("Outro", 100)])
            .await
            .expect("save");

        let got = repo.get_chapters(item).await.expect("get");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].chapter_index, 0);
        assert_eq!(got[1].chapter_index, 1);
        assert_eq!(got[0].name.as_deref(), Some("Intro"));

        // Re-saving fully replaces the previous set.
        repo.save_chapters(item, &[chapter("Only", 0)])
            .await
            .expect("resave");
        let got = repo.get_chapters(item).await.expect("get");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name.as_deref(), Some("Only"));
    }

    #[tokio::test]
    async fn get_single_and_delete() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = HermitChapterRepository::new(db);
        repo.save_chapters(item, &[chapter("Intro", 0), chapter("Mid", 50)])
            .await
            .expect("save");

        let one = repo.get_chapter(item, 1).await.expect("get_chapter");
        assert_eq!(one.expect("present").name.as_deref(), Some("Mid"));
        assert!(repo.get_chapter(item, 9).await.expect("miss").is_none());

        repo.delete_chapters(item).await.expect("delete");
        assert!(repo.get_chapters(item).await.expect("get").is_empty());
    }
}
