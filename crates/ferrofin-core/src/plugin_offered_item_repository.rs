//! The SQLite [`PluginOfferedItemStore`]: which library items the "Plugin
//! media analysis" pass has offered to each analyzer plugin, in the
//! Ferrofin-owned `FerrofinPluginOfferedItems` table (migration 0049). The
//! pass reads it back through the anti-join `translate_query` adds for
//! `InternalItemsQuery::not_offered_to_plugin`.
//!
//! Both ids are stored as `BaseItems."Id"` is (uppercase, hyphenated). Every
//! write selects its items from `BaseItems`, so an item deleted meanwhile is
//! skipped rather than breaking the foreign key.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::PluginOfferedItemStore;
use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;

/// The database-backed [`PluginOfferedItemStore`].
#[derive(Clone)]
pub struct FerrofinPluginOfferedItemStore {
    db: Database,
}

impl std::fmt::Debug for FerrofinPluginOfferedItemStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinPluginOfferedItemStore")
            .finish_non_exhaustive()
    }
}

impl FerrofinPluginOfferedItemStore {
    /// A store over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

/// `values` as a JSON array of strings, for one `json_each` bind: one
/// statement text whatever the count, so it stays in the statement cache.
fn json_list(values: impl IntoIterator<Item = String>) -> Result<String, ServiceError> {
    serde_json::to_string(&values.into_iter().collect::<Vec<_>>())
        .map_err(ServiceError::backend_source)
}

#[async_trait]
impl PluginOfferedItemStore for FerrofinPluginOfferedItemStore {
    async fn mark_offered(&self, plugin_id: Uuid, item_ids: &[Uuid]) -> Result<(), ServiceError> {
        if item_ids.is_empty() {
            return Ok(());
        }
        sqlx::query(
            r#"INSERT OR IGNORE INTO "FerrofinPluginOfferedItems" ("PluginId", "ItemId", "OfferedAt")
               SELECT ?1, bi."Id", ?2 FROM "BaseItems" AS bi
               WHERE bi."Id" IN (SELECT value FROM json_each(?3))"#,
        )
        .bind(guid_to_db(plugin_id))
        .bind(datetime_to_db(Utc::now()))
        .bind(json_list(item_ids.iter().map(|id| guid_to_db(*id)))?)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn mark_offered_created_between(
        &self,
        plugin_id: Uuid,
        kinds: &[BaseItemKind],
        created_from: Option<DateTime<Utc>>,
        created_before: DateTime<Utc>,
        include_undated: bool,
        except: &[Uuid],
    ) -> Result<u64, ServiceError> {
        let types: Vec<String> = kinds
            .iter()
            .filter_map(|kind| stored_type_name(*kind))
            .map(ToOwned::to_owned)
            .collect();
        if types.is_empty() {
            return Ok(0);
        }
        // One statement, so it holds the write lock for one pass over the
        // items of these kinds and no longer.
        let done = sqlx::query(
            r#"INSERT OR IGNORE INTO "FerrofinPluginOfferedItems" ("PluginId", "ItemId", "OfferedAt")
               SELECT ?1, bi."Id", ?2 FROM "BaseItems" AS bi
               WHERE bi."Type" IN (SELECT value FROM json_each(?3))
                 AND ((bi."DateCreated" < ?4 AND (?7 IS NULL OR bi."DateCreated" >= ?7))
                      OR (?5 AND bi."DateCreated" IS NULL))
                 AND bi."Id" NOT IN (SELECT value FROM json_each(?6))"#,
        )
        .bind(guid_to_db(plugin_id))
        .bind(datetime_to_db(Utc::now()))
        .bind(json_list(types)?)
        .bind(datetime_to_db(created_before))
        .bind(include_undated)
        .bind(json_list(except.iter().map(|id| guid_to_db(*id)))?)
        .bind(created_from.map(datetime_to_db))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected())
    }

    async fn forget_plugin(&self, plugin_id: Uuid) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "FerrofinPluginOfferedItems" WHERE "PluginId" = ?1"#)
            .bind(guid_to_db(plugin_id))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_traits::options::InternalItemsQuery;
    use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
    use sqlx::{Execute, Row};

    const PLUGIN: Uuid = Uuid::from_u128(0xA11A);
    const OTHER_PLUGIN: Uuid = Uuid::from_u128(0xB22B);
    /// Item `n` of a test is `Uuid::from_u128(ITEMS + n)`, clear of the
    /// placeholder row every database holds (`…0001`).
    const ITEMS: u128 = 0x1_0000;

    /// Saves one item of `kind` per `(id, date_created µs)`.
    async fn seed(db: &Database, kind: BaseItemKind, items: &[(u128, Option<i64>)]) {
        let rows: Vec<BaseItemEntity> = items
            .iter()
            .map(|&(id, date)| BaseItemEntity {
                id: guid_to_db(Uuid::from_u128(ITEMS + id)),
                type_: stored_type_name(kind).expect("stored kind").to_owned(),
                path: Some(format!("/media/{id:x}.mkv")),
                date_created: date.and_then(DateTime::from_timestamp_micros),
                ..Default::default()
            })
            .collect();
        crate::FerrofinItemPersistenceService::new(db.clone())
            .save_items(&rows)
            .await
            .expect("save items");
    }

    /// The ids recorded for `plugin`, sorted.
    async fn offered(db: &Database, plugin: Uuid) -> Vec<Uuid> {
        let ids: Vec<String> = sqlx::query_scalar(
            r#"SELECT "ItemId" FROM "FerrofinPluginOfferedItems" WHERE "PluginId" = ?1
               ORDER BY "ItemId""#,
        )
        .bind(guid_to_db(plugin))
        .fetch_all(db.pool())
        .await
        .expect("offered ids");
        ids.iter()
            .map(|id| Uuid::parse_str(id).expect("stored id"))
            .collect()
    }

    fn ids(ids: &[u128]) -> Vec<Uuid> {
        ids.iter().map(|id| Uuid::from_u128(ITEMS + id)).collect()
    }

    /// The pass's item query: the movies not offered to `plugin` yet.
    fn not_offered(plugin: Uuid) -> InternalItemsQuery {
        InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            not_offered_to_plugin: Some(plugin),
            ..InternalItemsQuery::default()
        }
    }

    fn repository(db: &Database) -> crate::FerrofinItemRepository {
        crate::FerrofinItemRepository::new(
            db.clone(),
            std::sync::Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        )
    }

    #[tokio::test]
    async fn offered_items_leave_the_plugins_query_and_only_that_plugins() {
        let db = test_db().await;
        seed(
            &db,
            BaseItemKind::Movie,
            &[(1, Some(10)), (2, Some(20)), (3, None)],
        )
        .await;
        let store = FerrofinPluginOfferedItemStore::new(db.clone());
        let repository = repository(&db);
        let mut all = repository
            .get_item_ids(&not_offered(PLUGIN))
            .await
            .expect("query");
        all.sort();
        assert_eq!(all, ids(&[1, 2, 3]));

        // Twice, and with an id that is not in the library: recorded once, skipped.
        for _ in 0..2 {
            store
                .mark_offered(PLUGIN, &ids(&[1, 3, 0xDEAD]))
                .await
                .expect("mark");
        }
        assert_eq!(offered(&db, PLUGIN).await, ids(&[1, 3]));
        assert_eq!(
            repository
                .get_item_ids(&not_offered(PLUGIN))
                .await
                .expect("query"),
            ids(&[2])
        );
        assert_eq!(
            repository
                .get_item_ids(&not_offered(OTHER_PLUGIN))
                .await
                .expect("query")
                .len(),
            3,
            "each plugin has its own record"
        );
        store.mark_offered(PLUGIN, &[]).await.expect("nothing");
    }

    #[tokio::test]
    async fn the_first_offer_keeps_its_time() {
        let db = test_db().await;
        seed(&db, BaseItemKind::Movie, &[(1, Some(10))]).await;
        let store = FerrofinPluginOfferedItemStore::new(db.clone());
        store.mark_offered(PLUGIN, &ids(&[1])).await.expect("mark");
        let at = || async {
            sqlx::query_scalar::<_, String>(
                r#"SELECT "OfferedAt" FROM "FerrofinPluginOfferedItems""#,
            )
            .fetch_one(db.pool())
            .await
            .expect("offered at")
        };
        let first = at().await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store.mark_offered(PLUGIN, &ids(&[1])).await.expect("again");
        assert_eq!(at().await, first);
    }

    /// The switch from the old watermark: the items of the plugin's kinds
    /// created before the bound count as offered, the undated ones when asked,
    /// never the excepted ones or another kind's, and a second run adds nothing.
    #[tokio::test]
    async fn items_created_before_the_bound_are_recorded_except_the_excepted() {
        let db = test_db().await;
        seed(
            &db,
            BaseItemKind::Movie,
            &[
                (1, Some(10)),
                (2, Some(19)),
                (3, Some(20)),
                (4, None),
                (5, Some(5)),
            ],
        )
        .await;
        seed(&db, BaseItemKind::Episode, &[(6, Some(1))]).await;
        let store = FerrofinPluginOfferedItemStore::new(db.clone());
        let bound = DateTime::from_timestamp_micros(20).expect("bound");
        let added = store
            .mark_offered_created_between(
                PLUGIN,
                &[BaseItemKind::Movie],
                None,
                bound,
                true,
                &ids(&[5]),
            )
            .await
            .expect("mark");
        assert_eq!(added, 3);
        assert_eq!(offered(&db, PLUGIN).await, ids(&[1, 2, 4]));
        let again = store
            .mark_offered_created_between(PLUGIN, &[BaseItemKind::Movie], None, bound, true, &[])
            .await
            .expect("again");
        assert_eq!(again, 1, "only the item no longer excepted");

        let added = store
            .mark_offered_created_between(
                OTHER_PLUGIN,
                &[BaseItemKind::Movie],
                None,
                bound,
                false,
                &[],
            )
            .await
            .expect("mark");
        assert_eq!(added, 3);
        assert_eq!(offered(&db, OTHER_PLUGIN).await, ids(&[1, 2, 5]));
        assert_eq!(
            store
                .mark_offered_created_between(PLUGIN, &[], None, bound, true, &[])
                .await
                .expect("no kinds"),
            0
        );
    }

    /// A later switch (the watermark moved on beside this version) takes only
    /// the items from its lower bound on, never an undated one.
    #[tokio::test]
    async fn a_lower_bound_leaves_the_earlier_and_undated_items() {
        let db = test_db().await;
        seed(
            &db,
            BaseItemKind::Movie,
            &[(1, Some(10)), (2, Some(20)), (3, Some(30)), (4, None)],
        )
        .await;
        let store = FerrofinPluginOfferedItemStore::new(db.clone());
        let at = |micros| DateTime::from_timestamp_micros(micros).expect("time");
        let added = store
            .mark_offered_created_between(
                PLUGIN,
                &[BaseItemKind::Movie],
                Some(at(20)),
                at(31),
                false,
                &[],
            )
            .await
            .expect("mark");
        assert_eq!(added, 2);
        assert_eq!(offered(&db, PLUGIN).await, ids(&[2, 3]));
    }

    #[tokio::test]
    async fn forgetting_a_plugin_keeps_the_others_offers() {
        let db = test_db().await;
        seed(&db, BaseItemKind::Movie, &[(1, Some(10)), (2, Some(20))]).await;
        let store = FerrofinPluginOfferedItemStore::new(db.clone());
        store
            .mark_offered(PLUGIN, &ids(&[1, 2]))
            .await
            .expect("mark");
        store
            .mark_offered(OTHER_PLUGIN, &ids(&[2]))
            .await
            .expect("mark");
        store.forget_plugin(PLUGIN).await.expect("forget");
        assert_eq!(offered(&db, PLUGIN).await, Vec::<Uuid>::new());
        assert_eq!(offered(&db, OTHER_PLUGIN).await, ids(&[2]));
    }

    /// The anti-join is one probe of the offered table's primary key per item,
    /// never a scan of the table.
    #[tokio::test]
    async fn not_offered_items_probe_the_offered_key() {
        let db = test_db().await;
        let mut builder = crate::translate_query::build_query(
            &InternalItemsQuery {
                order_by: vec![(
                    ferrofin_model::live_tv::ItemSortBy::DateCreated,
                    ferrofin_model::dto::SortOrder::Ascending,
                )],
                ..not_offered(PLUGIN)
            },
            crate::translate_query::QueryShape::IdsOnly,
        );
        let mut query = builder.build();
        let args = query.take_arguments().expect("arguments").expect("binds");
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
        let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_all(db.pool())
            .await
            .expect("explain query plan")
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect();
        assert!(
            plan.iter()
                .any(|d| d == "SEARCH po USING PRIMARY KEY (ItemId=? AND PluginId=?)"),
            "{plan:#?}"
        );
        assert!(!plan.iter().any(|d| d.starts_with("SCAN po")), "{plan:#?}");
    }

    /// Deleting an item deletes its rows, so an item brought back later is
    /// offered again.
    #[tokio::test]
    async fn a_deleted_items_offers_go_with_it() {
        let db = test_db().await;
        seed(&db, BaseItemKind::Movie, &[(1, Some(10)), (2, Some(20))]).await;
        let store = FerrofinPluginOfferedItemStore::new(db.clone());
        store
            .mark_offered(PLUGIN, &ids(&[1, 2]))
            .await
            .expect("mark");
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(Uuid::from_u128(ITEMS + 1)))
            .execute(db.writer())
            .await
            .expect("delete");
        assert_eq!(offered(&db, PLUGIN).await, ids(&[2]));
    }

    #[tokio::test]
    async fn a_closed_database_is_an_error() {
        let db = test_db().await;
        let store = FerrofinPluginOfferedItemStore::new(db.clone());
        db.close().await;
        assert!(store.mark_offered(PLUGIN, &ids(&[1])).await.is_err());
        assert!(
            store
                .mark_offered_created_between(
                    PLUGIN,
                    &[BaseItemKind::Movie],
                    None,
                    Utc::now(),
                    true,
                    &[]
                )
                .await
                .is_err()
        );
        assert!(store.forget_plugin(PLUGIN).await.is_err());
        let _ = format!("{store:?}");
    }
}
