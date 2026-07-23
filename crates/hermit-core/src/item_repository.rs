//! [`HermitItemRepository`] — the concrete [`ItemRepository`] over `hermit-db`.
//!
//! Port of `BaseItemRepository` (the `Querying`/`ByName` partials). Reads and
//! queries `BaseItems` rows, materializing [`BaseItemEntity`] instead of the
//! un-ported C# `BaseItem` domain object (per the persistence-trait port rules).
//! The query translation lives in [`crate::translate_query`]; this type wires it
//! to the pool and runs the resulting statements.
//!
//! The `ConfigurationManager` is a constructor dependency in C# but only feeds
//! path normalization that is not needed for the row-level reads here, so it is
//! not taken as a field (it would be injected at the composition root if a later
//! method needs it).

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_db::enums::ItemValueType;
use hermit_model::data::{BaseItemKind, CollectionType};
use hermit_model::entities::MediaStreamType;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::options::InternalItemsQuery;
use hermit_traits::persistence::{ItemRepository, ItemTypeLookup, ItemWithCounts};

use crate::db_error::{db_err, media_stream_type_disc};
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::{PLACEHOLDER_ID, QueryShape, build_query};

/// The concrete item repository.
///
/// Holds a cheaply-cloneable [`Database`] handle plus the shared
/// [`ItemTypeLookup`] (injected so the composition root can share one instance).
#[derive(Clone)]
pub struct HermitItemRepository {
    db: Database,
    item_type_lookup: Arc<dyn ItemTypeLookup>,
}

impl std::fmt::Debug for HermitItemRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitItemRepository")
            .finish_non_exhaustive()
    }
}

impl HermitItemRepository {
    /// Creates a repository over the given database and kind-lookup tables.
    #[must_use]
    pub fn new(db: Database, item_type_lookup: Arc<dyn ItemTypeLookup>) -> Self {
        Self {
            db,
            item_type_lookup,
        }
    }

    /// Runs a translated query in the requested shape, returning full rows.
    async fn fetch_rows(
        &self,
        filter: &InternalItemsQuery,
        shape: QueryShape,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let mut qb = build_query(filter, shape);
        let rows = qb
            .build_query_as::<BaseItemEntity>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Runs a translated query returning only the id column.
    async fn fetch_ids(&self, filter: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        let mut qb = build_query(filter, QueryShape::IdsOnly);
        let ids: Vec<String> = qb
            .build_query_scalar::<String>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect())
    }

    /// Runs a `COUNT(*)` over the translated query.
    async fn fetch_count(&self, filter: &InternalItemsQuery) -> Result<i32, ServiceError> {
        let mut qb = build_query(filter, QueryShape::Count);
        let count: i64 = qb
            .build_query_scalar::<i64>()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
    }

    /// Distinct `ItemValues.Value`s of the given types, optionally scoped to items
    /// of certain stored `Type`s (C# `GetItemValueNames`).
    async fn item_value_names(
        &self,
        types: &[ItemValueType],
        with_item_types: &[&str],
        exclude_item_types: &[&str],
    ) -> Result<Vec<String>, ServiceError> {
        let type_ints: Vec<i64> = types.iter().map(|t| i64::from(i32::from(*t))).collect();
        let mut sql = String::from(
            r#"SELECT DISTINCT iv."Value" FROM "ItemValuesMap" ivm
               JOIN "ItemValues" iv ON iv."ItemValueId" = ivm."ItemValueId"
               JOIN "BaseItems" bi ON bi."Id" = ivm."ItemId"
               WHERE iv."Type" IN ("#,
        );
        sql.push_str(&placeholders(type_ints.len()));
        sql.push(')');
        if !with_item_types.is_empty() {
            sql.push_str(r#" AND bi."Type" IN ("#);
            sql.push_str(&placeholders(with_item_types.len()));
            sql.push(')');
        }
        if !exclude_item_types.is_empty() {
            sql.push_str(r#" AND bi."Type" NOT IN ("#);
            sql.push_str(&placeholders(exclude_item_types.len()));
            sql.push(')');
        }
        sql.push_str(r#" ORDER BY iv."Value""#);

        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for t in &type_ints {
            query = query.bind(*t);
        }
        for t in with_item_types {
            query = query.bind((*t).to_owned());
        }
        for t in exclude_item_types {
            query = query.bind((*t).to_owned());
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    /// Resolves the by-name items of `kind` to [`ItemWithCounts`], counting the
    /// items that reference each by-name item's clean name via `ItemValues` of the
    /// given types (a pragmatic port of C# `GetItemValues`).
    async fn item_values_with_counts(
        &self,
        kind: BaseItemKind,
        value_types: &[ItemValueType],
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        let Some(type_name) = stored_type_name(kind) else {
            return Ok(QueryResult::default());
        };
        let by_name_query = InternalItemsQuery {
            include_item_types: vec![kind],
            ..Default::default()
        };
        let rows = self
            .fetch_rows(&by_name_query, QueryShape::FullRows)
            .await?;
        let _ = type_name;

        let mut items = Vec::with_capacity(rows.len());
        for item in rows {
            let clean = item.clean_name.clone().unwrap_or_default();
            let count = self.count_referencing_items(value_types, &clean).await?;
            let counts = hermit_model::dto::ItemCounts {
                item_count: count,
                ..Default::default()
            };
            items.push(ItemWithCounts { item, counts });
        }
        Ok(QueryResult::from_items(items))
    }

    /// Counts items that reference `clean_value` through an `ItemValues` row of any
    /// of `types`.
    async fn count_referencing_items(
        &self,
        types: &[ItemValueType],
        clean_value: &str,
    ) -> Result<i32, ServiceError> {
        let type_ints: Vec<i64> = types.iter().map(|t| i64::from(i32::from(*t))).collect();
        let mut sql = String::from(
            r#"SELECT COUNT(DISTINCT ivm."ItemId") FROM "ItemValuesMap" ivm
               JOIN "ItemValues" iv ON iv."ItemValueId" = ivm."ItemValueId"
               WHERE iv."CleanValue" = ? AND iv."Type" IN ("#,
        );
        sql.push_str(&placeholders(type_ints.len()));
        sql.push(')');
        let mut query = sqlx::query_scalar::<_, i64>(&sql).bind(clean_value.to_owned());
        for t in &type_ints {
            query = query.bind(*t);
        }
        let count = query.fetch_one(self.db.pool()).await.map_err(db_err)?;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
    }
}

/// The `ItemValues` types treated as "genre" (C# `_getGenreValueTypes`).
const GENRE_TYPES: &[ItemValueType] = &[ItemValueType::Genre];
/// The `ItemValues` types treated as "studio" (C# `_getStudiosValueTypes`).
const STUDIO_TYPES: &[ItemValueType] = &[ItemValueType::Studios];
/// The `ItemValues` types treated as "artist" (C# `_getArtistValueTypes`).
const ARTIST_TYPES: &[ItemValueType] = &[ItemValueType::Artist];
/// The `ItemValues` types treated as "album artist" (C# `_getAlbumArtistValueTypes`).
const ALBUM_ARTIST_TYPES: &[ItemValueType] = &[ItemValueType::AlbumArtist];
/// All artist-ish `ItemValues` types (C# `_getAllArtistsValueTypes`).
const ALL_ARTIST_TYPES: &[ItemValueType] = &[ItemValueType::Artist, ItemValueType::AlbumArtist];

#[async_trait]
impl ItemRepository for HermitItemRepository {
    async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id.is_nil() {
            return Err(ServiceError::invalid_input("item id can't be empty"));
        }
        let row = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1 AND "Id" <> ?2"#,
        )
        .bind(id.to_string())
        .bind(PLACEHOLDER_ID)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row)
    }

    async fn get_items(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        let items = self.fetch_rows(filter, QueryShape::FullRows).await?;
        let start_index = filter.start_index.unwrap_or(0);
        let total =
            if filter.enable_total_record_count && (filter.limit.is_some() || start_index > 0) {
                self.fetch_count(filter).await?
            } else {
                i32::try_from(items.len()).unwrap_or(i32::MAX) + start_index
            };
        Ok(QueryResult::new(Some(start_index), Some(total), items))
    }

    async fn get_item_ids(&self, filter: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        self.fetch_ids(filter).await
    }

    async fn get_item_list(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.fetch_rows(filter, QueryShape::FullRows).await
    }

    async fn get_latest_item_list(
        &self,
        filter: &InternalItemsQuery,
        collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // Only movies/tvshows/music support the Latest API (C# early exit).
        if !matches!(
            collection_type,
            CollectionType::movies | CollectionType::tvshows | CollectionType::music
        ) {
            return Ok(Vec::new());
        }
        // The smart Season/Series container selection is deferred (library
        // manager); the base behavior returns the filtered rows newest-first.
        let mut latest = filter.clone();
        latest.order_by = vec![(
            hermit_model::live_tv::ItemSortBy::DateCreated,
            hermit_model::dto::SortOrder::Descending,
        )];
        self.fetch_rows(&latest, QueryShape::FullRows).await
    }

    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        let exists: Option<i64> =
            sqlx::query_scalar(r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(id.to_string())
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(exists.is_some())
    }

    async fn get_genres(
        &self,
        _filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(BaseItemKind::Genre, GENRE_TYPES)
            .await
    }

    async fn get_music_genres(
        &self,
        _filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(BaseItemKind::MusicGenre, GENRE_TYPES)
            .await
    }

    async fn get_studios(
        &self,
        _filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(BaseItemKind::Studio, STUDIO_TYPES)
            .await
    }

    async fn get_artists(
        &self,
        _filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(BaseItemKind::MusicArtist, ARTIST_TYPES)
            .await
    }

    async fn get_album_artists(
        &self,
        _filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(BaseItemKind::MusicArtist, ALBUM_ARTIST_TYPES)
            .await
    }

    async fn get_all_artists(
        &self,
        _filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(BaseItemKind::MusicArtist, ALL_ARTIST_TYPES)
            .await
    }

    async fn get_music_genre_names(&self) -> Result<Vec<String>, ServiceError> {
        let music_types = self.item_type_lookup.music_genre_types();
        let with: Vec<&str> = music_types.iter().map(String::as_str).collect();
        self.item_value_names(GENRE_TYPES, &with, &[]).await
    }

    async fn get_studio_names(&self) -> Result<Vec<String>, ServiceError> {
        self.item_value_names(STUDIO_TYPES, &[], &[]).await
    }

    async fn get_genre_names(&self) -> Result<Vec<String>, ServiceError> {
        let music_types = self.item_type_lookup.music_genre_types();
        let exclude: Vec<&str> = music_types.iter().map(String::as_str).collect();
        self.item_value_names(GENRE_TYPES, &[], &exclude).await
    }

    async fn get_all_artist_names(&self) -> Result<Vec<String>, ServiceError> {
        self.item_value_names(ALL_ARTIST_TYPES, &[], &[]).await
    }

    async fn get_media_stream_languages(
        &self,
        filter: &InternalItemsQuery,
        stream_type: MediaStreamType,
    ) -> Result<Vec<String>, ServiceError> {
        // Restrict the item set with the filter, then collect distinct stream
        // languages of the requested type ("und" for missing), matching C#.
        let ids = self.fetch_ids(filter).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let stream_disc = i64::from(media_stream_type_disc(stream_type));
        let mut sql = String::from(
            r#"SELECT DISTINCT CASE WHEN ms."Language" IS NULL OR ms."Language" = ''
                 THEN 'und' ELSE ms."Language" END
               FROM "MediaStreamInfos" ms WHERE ms."StreamType" = ? AND ms."ItemId" IN ("#,
        );
        sql.push_str(&placeholders(ids.len()));
        sql.push(')');
        let mut query = sqlx::query_scalar::<_, String>(&sql).bind(stream_disc);
        for id in &ids {
            query = query.bind(id.to_string());
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    async fn get_query_filters_legacy(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        let ids = self.fetch_ids(filter).await?;
        if ids.is_empty() {
            return Ok(QueryFiltersLegacy::default());
        }
        let id_strings: Vec<String> = ids.iter().map(Uuid::to_string).collect();

        let years = self.distinct_years(&id_strings).await?;
        let official_ratings = self.distinct_official_ratings(&id_strings).await?;
        let genres = self
            .distinct_item_values(&id_strings, ItemValueType::Genre)
            .await?;
        let tags = self
            .distinct_item_values(&id_strings, ItemValueType::Tags)
            .await?;

        Ok(QueryFiltersLegacy {
            genres,
            tags,
            official_ratings,
            years,
        })
    }

    async fn get_is_played(
        &self,
        user: &UserEntity,
        id: Uuid,
        recursive: bool,
    ) -> Result<bool, ServiceError> {
        // Non-recursive: all direct, non-virtual leaf children played by the user.
        // Recursive descent (via the AncestorIds/LinkedChildren closure) is the
        // library manager's job; here the direct-children form is honored and the
        // recursive flag widens to the ancestor closure where present.
        let uid = user.id.clone();
        // Both forms select leaf descendants of `id`; they differ only in how a
        // child is related to the parent. Each contributes a FROM fragment (join)
        // and a scope predicate that folds into the single WHERE below — never a
        // second WHERE.
        let (join, scope) = if recursive {
            // Any descendant, via the AncestorIds closure.
            (
                r#"JOIN "AncestorIds" a ON a."ItemId" = bi."Id" AND a."ParentItemId" = ?1"#,
                "1 = 1",
            )
        } else {
            // Direct children only.
            ("", r#"bi."ParentId" = ?1"#)
        };
        let sql = format!(
            r#"SELECT NOT EXISTS (
                 SELECT 1 FROM "BaseItems" bi {join}
                 WHERE {scope} AND bi."IsFolder" = 0 AND bi."IsVirtualItem" = 0
                   AND NOT EXISTS (SELECT 1 FROM "UserData" ud
                       WHERE ud."ItemId" = bi."Id" AND ud."UserId" = ?2 AND ud."Played" = 1))"#,
        );
        let all_played: i64 = sqlx::query_scalar(&sql)
            .bind(id.to_string())
            .bind(uid)
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(all_played != 0)
    }
}

impl HermitItemRepository {
    /// Distinct positive production years of the given item ids, ascending.
    async fn distinct_years(&self, ids: &[String]) -> Result<Vec<i32>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT DISTINCT "ProductionYear" FROM "BaseItems"
               WHERE "ProductionYear" IS NOT NULL AND "ProductionYear" > 0 AND "Id" IN ("#,
        );
        sql.push_str(&placeholders(ids.len()));
        sql.push_str(r#") ORDER BY "ProductionYear""#);
        let mut query = sqlx::query_scalar::<_, i64>(&sql);
        for id in ids {
            query = query.bind(id.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|y| i32::try_from(y).unwrap_or(i32::MAX))
            .collect())
    }

    /// Distinct non-empty official ratings of the given item ids, ascending.
    async fn distinct_official_ratings(&self, ids: &[String]) -> Result<Vec<String>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT DISTINCT "OfficialRating" FROM "BaseItems"
               WHERE "OfficialRating" IS NOT NULL AND "OfficialRating" <> '' AND "Id" IN ("#,
        );
        sql.push_str(&placeholders(ids.len()));
        sql.push_str(r#") ORDER BY "OfficialRating""#);
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for id in ids {
            query = query.bind(id.clone());
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    /// Distinct display values of one `ItemValues` type over the given item ids.
    async fn distinct_item_values(
        &self,
        ids: &[String],
        value_type: ItemValueType,
    ) -> Result<Vec<String>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT DISTINCT iv."Value" FROM "ItemValuesMap" ivm
               JOIN "ItemValues" iv ON iv."ItemValueId" = ivm."ItemValueId"
               WHERE iv."Type" = ? AND ivm."ItemId" IN ("#,
        );
        sql.push_str(&placeholders(ids.len()));
        sql.push_str(r#") ORDER BY iv."Value""#);
        let mut query =
            sqlx::query_scalar::<_, String>(&sql).bind(i64::from(i32::from(value_type)));
        for id in ids {
            query = query.bind(id.clone());
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }
}

/// Builds a `?, ?, …` placeholder list of length `n` (at least one).
fn placeholders(n: usize) -> String {
    if n == 0 {
        return "NULL".to_owned();
    }
    let mut s = String::with_capacity(n * 3);
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('?');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::test_support::{
        seed_item, seed_item_genre, seed_named_item, seed_user, seed_user_data, test_db,
    };
    use hermit_db::Database;

    fn repo(db: &Database) -> HermitItemRepository {
        HermitItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
    }

    #[test]
    fn placeholders_shapes() {
        assert_eq!(placeholders(0), "NULL");
        assert_eq!(placeholders(1), "?");
        assert_eq!(placeholders(3), "?, ?, ?");
    }

    #[tokio::test]
    async fn item_exists_and_get_items_total_without_limit() {
        let db = test_db().await;
        let repository = repo(&db);
        let id = Uuid::from_u128(0x7001);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solo").await;

        assert!(repository.item_exists(id).await.expect("exists"));
        assert!(
            !repository
                .item_exists(Uuid::from_u128(0xBEEF))
                .await
                .expect("absent")
        );

        // No limit / no start_index → total is derived from the row count.
        let res = repository
            .get_items(&InternalItemsQuery::default())
            .await
            .expect("get_items");
        assert_eq!(res.total_record_count, 1);
        assert_eq!(res.items.len(), 1);
    }

    #[tokio::test]
    async fn genres_studios_artists_roll_up_with_counts() {
        let db = test_db().await;
        let repository = repo(&db);

        // A by-name Genre item whose clean name matches a movie's genre value.
        let genre_id = Uuid::from_u128(0x8001);
        seed_named_item(&db, genre_id, BaseItemKind::Genre, "Drama").await;
        crate::test_support::set_clean_name(&db, genre_id, "Drama").await;

        let movie = Uuid::from_u128(0x8002);
        seed_named_item(&db, movie, BaseItemKind::Movie, "A Drama Film").await;
        seed_item_genre(&db, movie, "Drama").await;

        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 1);
        assert_eq!(genres.items[0].counts.item_count, 1);

        // Studios / artists / album artists / all-artists resolve (empty here) and
        // music genres too — exercising every by-name entry point.
        assert!(
            repository
                .get_studios(&InternalItemsQuery::default())
                .await
                .expect("studios")
                .items
                .is_empty()
        );
        assert!(
            repository
                .get_artists(&InternalItemsQuery::default())
                .await
                .expect("artists")
                .items
                .is_empty()
        );
        assert!(
            repository
                .get_album_artists(&InternalItemsQuery::default())
                .await
                .expect("album artists")
                .items
                .is_empty()
        );
        assert!(
            repository
                .get_all_artists(&InternalItemsQuery::default())
                .await
                .expect("all artists")
                .items
                .is_empty()
        );
        assert!(
            repository
                .get_music_genres(&InternalItemsQuery::default())
                .await
                .expect("music genres")
                .items
                .is_empty()
        );
    }

    #[tokio::test]
    async fn value_name_lists_are_distinct_and_ordered() {
        let db = test_db().await;
        let repository = repo(&db);

        let movie = Uuid::from_u128(0x9001);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Genred").await;
        seed_item_genre(&db, movie, "Zeta").await;
        seed_item_genre(&db, movie, "Alpha").await;
        // A second item sharing "Alpha" must not duplicate it.
        let movie2 = Uuid::from_u128(0x9002);
        seed_named_item(&db, movie2, BaseItemKind::Movie, "Genred Two").await;
        seed_item_genre(&db, movie2, "Alpha").await;

        let names = repository.get_genre_names().await.expect("genre names");
        assert_eq!(names, vec!["Alpha".to_owned(), "Zeta".to_owned()]);

        // The remaining name lists execute their SQL (empty result sets).
        assert!(
            repository
                .get_studio_names()
                .await
                .expect("studios")
                .is_empty()
        );
        assert!(
            repository
                .get_all_artist_names()
                .await
                .expect("artists")
                .is_empty()
        );
        assert!(
            repository
                .get_music_genre_names()
                .await
                .expect("music genres")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn query_filters_legacy_collects_years_ratings_genres_tags() {
        let db = test_db().await;
        let repository = repo(&db);

        let movie = Uuid::from_u128(0xA001);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Rated").await;
        seed_item_genre(&db, movie, "Comedy").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "ProductionYear" = 1999, "OfficialRating" = 'PG-13'
               WHERE "Id" = ?1"#,
        )
        .bind(movie.to_string())
        .execute(db.pool())
        .await
        .expect("set year/rating");

        let filters = repository
            .get_query_filters_legacy(&InternalItemsQuery::default())
            .await
            .expect("filters");
        assert_eq!(filters.years, vec![1999]);
        assert_eq!(filters.official_ratings, vec!["PG-13".to_owned()]);
        assert_eq!(filters.genres, vec!["Comedy".to_owned()]);
        assert!(filters.tags.is_empty());

        // With no matching items the filters come back empty (early return).
        let none = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Book],
            ..Default::default()
        };
        let empty = repository
            .get_query_filters_legacy(&none)
            .await
            .expect("empty filters");
        assert!(empty.years.is_empty() && empty.genres.is_empty());
    }

    #[tokio::test]
    async fn media_stream_languages_dedup_and_default_und() {
        let db = test_db().await;
        let repository = repo(&db);

        let item = Uuid::from_u128(0xB001);
        seed_item(&db, item, BaseItemKind::Movie).await;
        // Two audio streams: one English, one with no language → 'und'.
        for (idx, lang) in [(0_i64, Some("eng")), (1, None)] {
            sqlx::query(
                r#"INSERT INTO "MediaStreamInfos"
                   ("ItemId", "StreamIndex", "IsDefault", "IsExternal", "IsForced",
                    "IsOriginal", "StreamType", "Language")
                   VALUES (?1, ?2, 0, 0, 0, 0, 0, ?3)"#,
            )
            .bind(item.to_string())
            .bind(idx)
            .bind(lang)
            .execute(db.pool())
            .await
            .expect("insert stream");
        }

        let mut langs = repository
            .get_media_stream_languages(&InternalItemsQuery::default(), MediaStreamType::Audio)
            .await
            .expect("langs");
        langs.sort();
        assert_eq!(langs, vec!["eng".to_owned(), "und".to_owned()]);

        // No matching items → empty (early return before the stream query).
        let none = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Book],
            ..Default::default()
        };
        assert!(
            repository
                .get_media_stream_languages(&none, MediaStreamType::Audio)
                .await
                .expect("empty langs")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn is_played_reflects_direct_children_user_data() {
        let db = test_db().await;
        let repository = repo(&db);

        let parent = Uuid::from_u128(0xC001);
        seed_item(&db, parent, BaseItemKind::Series).await;
        let child = Uuid::from_u128(0xC002);
        seed_item(&db, child, BaseItemKind::Episode).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(child.to_string())
            .bind(parent.to_string())
            .execute(db.pool())
            .await
            .expect("set parent");

        let user = seed_user(&db, Uuid::from_u128(0xC0DE)).await;
        seed_user_data(&db, Uuid::from_u128(0xC0DE), child, true, None).await;

        // The recursive branch runs its AncestorIds join. With an ancestor row
        // present, the played child makes the closure fully played.
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(child.to_string())
            .bind(parent.to_string())
            .execute(db.pool())
            .await
            .expect("ancestor");
        assert!(
            repository
                .get_is_played(&user, parent, true)
                .await
                .expect("is_played recursive")
        );

        // Non-recursive branch: the played child is a direct child of `parent`
        // (its ParentId was set above), so the direct-children closure is fully
        // played.
        assert!(
            repository
                .get_is_played(&user, parent, false)
                .await
                .expect("is_played non-recursive")
        );

        // And an UNplayed direct child makes it not-all-played.
        let unplayed = Uuid::from_u128(0xBEEF);
        seed_item(&db, unplayed, BaseItemKind::Episode).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(unplayed.to_string())
            .bind(parent.to_string())
            .execute(db.pool())
            .await
            .expect("set parent of unplayed");
        assert!(
            !repository
                .get_is_played(&user, parent, false)
                .await
                .expect("is_played non-recursive with unplayed child")
        );
    }

    #[tokio::test]
    async fn latest_item_list_gates_on_collection_type() {
        let db = test_db().await;
        let repository = repo(&db);
        seed_named_item(&db, Uuid::from_u128(0xD001), BaseItemKind::Movie, "New").await;

        // A supported collection type returns rows newest-first.
        let movies = repository
            .get_latest_item_list(&InternalItemsQuery::default(), CollectionType::movies)
            .await
            .expect("latest movies");
        assert_eq!(movies.len(), 1);

        // An unsupported collection type early-returns empty.
        let books = repository
            .get_latest_item_list(&InternalItemsQuery::default(), CollectionType::books)
            .await
            .expect("latest books");
        assert!(books.is_empty());
    }
}
