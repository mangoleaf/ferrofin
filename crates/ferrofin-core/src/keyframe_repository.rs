//! [`FerrofinKeyframeRepository`] — the concrete [`KeyframeRepository`] over
//! `ferrofin-db`.
//!
//! Port of `KeyframeRepository`. The `KeyframeData` table holds one row per item
//! (its primary key is `ItemId`). In C# `SaveKeyframeDataAsync` deletes then
//! re-inserts inside a transaction; that is preserved so a save fully replaces
//! the item's keyframe row. The C# `Map` between the entity and the
//! `MediaEncoding.Keyframes.KeyframeData` DTO is a DTO-layer concern; the trait
//! works directly on [`KeyframeDataEntity`] rows, so no mapping is done here (the
//! `KeyframeTicks` JSON stays as its raw string).

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::KeyframeDataEntity;
use ferrofin_db::store::guid_to_db;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::KeyframeRepository;

use crate::db_error::db_err;

/// The concrete keyframe repository.
#[derive(Clone)]
pub struct FerrofinKeyframeRepository {
    db: Database,
}

impl std::fmt::Debug for FerrofinKeyframeRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinKeyframeRepository")
            .finish_non_exhaustive()
    }
}

impl FerrofinKeyframeRepository {
    /// Creates a keyframe repository over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl KeyframeRepository for FerrofinKeyframeRepository {
    async fn get_keyframe_data(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<KeyframeDataEntity>, ServiceError> {
        let rows = sqlx::query_as::<_, KeyframeDataEntity>(
            r#"SELECT * FROM "KeyframeData" WHERE "ItemId" = ?1"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn save_keyframe_data(
        &self,
        item_id: Uuid,
        data: &KeyframeDataEntity,
    ) -> Result<(), ServiceError> {
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "KeyframeData" WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item_id))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            r#"INSERT INTO "KeyframeData" ("ItemId", "KeyframeTicks", "TotalDuration")
               VALUES (?1, ?2, ?3)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(&data.keyframe_ticks)
        .bind(data.total_duration)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn delete_keyframe_data(&self, item_id: Uuid) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "KeyframeData" WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item_id))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FerrofinKeyframeRepository;
    use crate::test_support::{seed_item, test_db};
    use ferrofin_db::entities::base_items::KeyframeDataEntity;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::persistence::KeyframeRepository;
    use uuid::Uuid;

    #[tokio::test]
    async fn save_get_and_delete_keyframes() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinKeyframeRepository::new(db);

        let data = KeyframeDataEntity {
            item_id: item.to_string(),
            keyframe_ticks: Some("[0,100,200]".to_owned()),
            total_duration: 300,
        };
        repo.save_keyframe_data(item, &data).await.expect("save");

        let got = repo.get_keyframe_data(item).await.expect("get");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].total_duration, 300);
        assert_eq!(got[0].keyframe_ticks.as_deref(), Some("[0,100,200]"));

        // A second save replaces the single row rather than duplicating it.
        let data2 = KeyframeDataEntity {
            total_duration: 500,
            ..data
        };
        repo.save_keyframe_data(item, &data2).await.expect("resave");
        let got = repo.get_keyframe_data(item).await.expect("get");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].total_duration, 500);

        repo.delete_keyframe_data(item).await.expect("delete");
        assert!(repo.get_keyframe_data(item).await.expect("get").is_empty());
    }
}
