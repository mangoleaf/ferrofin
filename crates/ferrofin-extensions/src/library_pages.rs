//! Library walks a page at a time.
//!
//! Intro Skipper's queue and Merge Versions walk every Episode or Movie of the
//! library while they do slow work per item (ffprobe, merges). Upstream holds
//! the whole list. Here a walk takes a snapshot of the matching ids
//! (`get_item_ids`, the list's own statement with an ids-only projection) and
//! reads the rows by id, [`PAGE`] at a time, so a large library is never held
//! whole. Unlike `start_index` paging, a snapshot keeps the list's order and
//! cannot drop a row when a scan deletes another one mid-walk.

use std::collections::HashMap;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::options::InternalItemsQuery;
use uuid::Uuid;

/// Rows read per page: the library tasks' page
/// ([`ferrofin_core::scheduled_tasks::library::PAGE_SIZE`]).
#[allow(clippy::cast_sign_loss)] // a positive constant
pub(crate) const PAGE: usize = ferrofin_core::scheduled_tasks::library::PAGE_SIZE as usize;

/// The rows of `ids`, one page of a `get_item_ids(query)` snapshot, that still
/// match `query`, in `ids`' order. A row deleted or changed since the snapshot
/// drops out, as it would from a fresh list. `query` is the snapshot's own,
/// unpaged query: a `limit` or `start_index` would apply to every page.
///
/// # Errors
/// The read failed.
pub(crate) async fn rows(
    library: &dyn LibraryManager,
    query: &InternalItemsQuery,
    ids: &[Uuid],
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    // An empty id list is no `Id IN` filter at all: the whole library.
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let position: HashMap<Uuid, usize> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();
    let mut rows = library
        .get_item_list(&InternalItemsQuery {
            item_ids: ids.to_vec(),
            ..query.clone()
        })
        .await?;
    rows.sort_by_key(|row| {
        Uuid::parse_str(&row.id)
            .ok()
            .and_then(|id| position.get(&id).copied())
    });
    Ok(rows)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::{ImageType, MediaStreamType};
    use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::options::{InternalItemsQuery, ItemImageInfo};
    use ferrofin_traits::persistence::{
        FolderChild, ItemPersistenceService as _, ItemRepository, ItemWithCounts,
        PlaylistItemsWithAccess,
    };
    use uuid::Uuid;

    use super::{PAGE, rows};

    /// An [`ItemRepository`] over the real one that refuses to return more
    /// than one page of rows from a single list read. The walks do slow work
    /// per item, so one that reads the whole library at once holds every row
    /// throughout; over this fake it fails instead. Every other method,
    /// defaulted ones included, forwards to the real repository: a method
    /// added to [`ItemRepository`] must be forwarded here too, or tests over
    /// this fake run its default body instead of the real one.
    pub(crate) struct OnePageAtATime(pub(crate) Arc<dyn ItemRepository>);

    fn one_page(rows: Vec<BaseItemEntity>) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if rows.len() > PAGE {
            return Err(ServiceError::backend(format!(
                "{} rows in one read: page the walk",
                rows.len()
            )));
        }
        Ok(rows)
    }

    #[async_trait]
    impl ItemRepository for OnePageAtATime {
        async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
            self.0.retrieve_item(id).await
        }
        async fn retrieve_items(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError> {
            one_page(self.0.retrieve_items(ids).await?)
        }
        async fn visible_library_ids(&self, user: &UserEntity) -> Result<Vec<Uuid>, ServiceError> {
            self.0.visible_library_ids(user).await
        }
        async fn locked_item_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
            self.0.locked_item_ids().await
        }
        async fn get_ancestor_chain(
            &self,
            item_id: Uuid,
        ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
            self.0.get_ancestor_chain(item_id).await
        }
        async fn get_items(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
            self.0.get_items(filter).await
        }
        async fn get_item_ids(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            self.0.get_item_ids(filter).await
        }
        async fn get_item_list(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            one_page(self.0.get_item_list(filter).await?)
        }
        async fn get_latest_item_list(
            &self,
            filter: &InternalItemsQuery,
            collection_type: ferrofin_model::data::CollectionType,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            self.0.get_latest_item_list(filter, collection_type).await
        }
        async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
            self.0.item_exists(id).await
        }
        async fn get_items_by_primary_version(
            &self,
            primary_id: Uuid,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            self.0.get_items_by_primary_version(primary_id).await
        }
        async fn get_items_with_provider_id(
            &self,
            provider_key: &str,
        ) -> Result<Vec<(Uuid, String)>, ServiceError> {
            self.0.get_items_with_provider_id(provider_key).await
        }
        async fn get_image_infos(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError> {
            self.0.get_image_infos(item_id).await
        }
        async fn swap_item_images(
            &self,
            item_id: Uuid,
            image_type: ImageType,
            index1: i32,
            index2: i32,
        ) -> Result<bool, ServiceError> {
            self.0
                .swap_item_images(item_id, image_type, index1, index2)
                .await
        }
        async fn get_genres(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
            self.0.get_genres(filter).await
        }
        async fn get_music_genres(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
            self.0.get_music_genres(filter).await
        }
        async fn get_studios(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
            self.0.get_studios(filter).await
        }
        async fn get_artists(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
            self.0.get_artists(filter).await
        }
        async fn get_album_artists(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
            self.0.get_album_artists(filter).await
        }
        async fn get_all_artists(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
            self.0.get_all_artists(filter).await
        }
        async fn get_music_genre_names(&self) -> Result<Vec<String>, ServiceError> {
            self.0.get_music_genre_names().await
        }
        async fn get_studio_names(&self) -> Result<Vec<String>, ServiceError> {
            self.0.get_studio_names().await
        }
        async fn get_genre_names(&self) -> Result<Vec<String>, ServiceError> {
            self.0.get_genre_names().await
        }
        async fn get_all_artist_names(&self) -> Result<Vec<String>, ServiceError> {
            self.0.get_all_artist_names().await
        }
        async fn get_media_stream_languages(
            &self,
            filter: &InternalItemsQuery,
            stream_type: MediaStreamType,
        ) -> Result<Vec<String>, ServiceError> {
            self.0.get_media_stream_languages(filter, stream_type).await
        }
        async fn get_query_filters_legacy(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<QueryFiltersLegacy, ServiceError> {
            self.0.get_query_filters_legacy(filter).await
        }
        async fn get_is_played(
            &self,
            user: &UserEntity,
            id: Uuid,
            recursive: bool,
        ) -> Result<bool, ServiceError> {
            self.0.get_is_played(user, id, recursive).await
        }
        async fn get_playlist_items_with_access(
            &self,
            playlist_id: Uuid,
            user_id: Uuid,
            child_type: i32,
        ) -> Result<PlaylistItemsWithAccess, ServiceError> {
            self.0
                .get_playlist_items_with_access(playlist_id, user_id, child_type)
                .await
        }
        async fn filter_folder_children(
            &self,
            rows: Vec<BaseItemEntity>,
            filter: &InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            self.0.filter_folder_children(rows, filter).await
        }
        async fn sort_folder_children(
            &self,
            rows: Vec<BaseItemEntity>,
            filter: &InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            self.0.sort_folder_children(rows, filter).await
        }
        async fn get_folder_children(
            &self,
            query: &InternalItemsQuery,
        ) -> Result<Vec<FolderChild>, ServiceError> {
            self.0.get_folder_children(query).await
        }
        async fn get_extra_owner_ids_batch(
            &self,
            items: &[BaseItemEntity],
            grouped_series: &[Uuid],
        ) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
            self.0
                .get_extra_owner_ids_batch(items, grouped_series)
                .await
        }
        async fn build_access_filter(
            &self,
            user: &UserEntity,
        ) -> Result<Option<InternalItemsQuery>, ServiceError> {
            self.0.build_access_filter(user).await
        }
        async fn get_item_id_clean_names(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<Vec<(String, Option<String>)>, ServiceError> {
            self.0.get_item_id_clean_names(filter).await
        }
        async fn get_items_by_primary_version_batch(
            &self,
            primary_ids: &[Uuid],
        ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
            self.0.get_items_by_primary_version_batch(primary_ids).await
        }
        async fn get_items_by_primary_version_batch_flagged(
            &self,
            primary_ids: &[Uuid],
        ) -> Result<HashMap<Uuid, Vec<(BaseItemEntity, bool)>>, ServiceError> {
            self.0
                .get_items_by_primary_version_batch_flagged(primary_ids)
                .await
        }
        async fn get_merged_version_ids(
            &self,
            primary_id: Uuid,
        ) -> Result<Vec<Uuid>, ServiceError> {
            self.0.get_merged_version_ids(primary_id).await
        }
        async fn get_image_infos_batch(
            &self,
            item_ids: &[Uuid],
        ) -> Result<HashMap<Uuid, Vec<ItemImageInfo>>, ServiceError> {
            self.0.get_image_infos_batch(item_ids).await
        }
        async fn get_image_infos_to_serve(
            &self,
            item_id: Uuid,
        ) -> Result<ferrofin_traits::options::ItemImagesToServe, ServiceError> {
            self.0.get_image_infos_to_serve(item_id).await
        }
        async fn get_media_stream_languages_by_type(
            &self,
            filter: &InternalItemsQuery,
            stream_types: &[MediaStreamType],
        ) -> Result<HashMap<MediaStreamType, Vec<String>>, ServiceError> {
            self.0
                .get_media_stream_languages_by_type(filter, stream_types)
                .await
        }
        async fn get_distinct_years(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<Vec<i32>, ServiceError> {
            self.0.get_distinct_years(filter).await
        }
        async fn get_tag_names(
            &self,
            filter: &InternalItemsQuery,
        ) -> Result<Vec<String>, ServiceError> {
            self.0.get_tag_names(filter).await
        }
    }

    /// A page keeps the snapshot's order, drops what no longer matches the
    /// query (a deleted id, a row of another type), and an empty page reads
    /// nothing.
    #[tokio::test]
    async fn a_page_keeps_the_snapshot_order_and_drops_what_no_longer_matches() {
        let db = ferrofin_db::Database::connect_in_memory()
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrations");
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
        let persistence = Arc::new(ferrofin_core::FerrofinItemPersistenceService::new(
            db.clone(),
        ));
        let library = ferrofin_core::FerrofinLibraryManager::new(
            Arc::new(ferrofin_core::FerrofinItemRepository::new(
                db.clone(),
                lookup,
            )),
            Arc::new(ferrofin_core::FerrofinItemCountService::new(db.clone())),
            persistence.clone(),
            Arc::new(ferrofin_core::FerrofinPeopleRepository::new(db.clone())),
        );
        let row = |n: u128, kind: BaseItemKind| BaseItemEntity {
            id: guid_to_db(Uuid::from_u128(n)),
            type_: ferrofin_core::item_type_lookup::stored_type_name(kind)
                .expect("type")
                .to_owned(),
            ..BaseItemEntity::default()
        };
        persistence
            .save_items(&[
                row(0x21, BaseItemKind::Episode),
                row(0x22, BaseItemKind::Episode),
                row(0x23, BaseItemKind::Episode),
                row(0x24, BaseItemKind::Movie),
            ])
            .await
            .expect("seed");
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Episode],
            recursive: true,
            ..InternalItemsQuery::default()
        };
        // Not `…0001`: that is the placeholder row every query leaves out.
        let page = [0x23, 0x29, 0x24, 0x21].map(Uuid::from_u128);

        let read: Vec<String> = rows(&library, &query, &page)
            .await
            .expect("rows")
            .into_iter()
            .map(|row| row.id)
            .collect();

        assert_eq!(
            read,
            [
                guid_to_db(Uuid::from_u128(0x23)),
                guid_to_db(Uuid::from_u128(0x21))
            ]
        );
        let empty: Vec<String> = rows(&library, &query, &[])
            .await
            .expect("rows")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(empty, Vec::<String>::new(), "an empty page reads nothing");
    }
}
