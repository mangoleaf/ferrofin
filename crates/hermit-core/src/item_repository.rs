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
use chrono::{DateTime, Utc};
use hermit_db::Database;
use hermit_db::entities::base_items::{BaseItemEntity, BaseItemImageInfoEntity};
use hermit_db::entities::users::UserEntity;
use hermit_db::enums::ItemValueType;
use hermit_model::data::CollectionType;
use hermit_model::entities::ImageType;
use hermit_model::entities::MediaStreamType;
use hermit_model::querying::{QueryFiltersLegacy, QueryResult};
use hermit_traits::options::ItemImageInfo;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::options::InternalItemsQuery;
use hermit_traits::persistence::{ItemRepository, ItemTypeLookup, ItemWithCounts};

use crate::db_error::{db_err, media_stream_type_disc};
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::{PLACEHOLDER_ID, QueryShape, append_predicates, build_query};
use sqlx::{QueryBuilder, Sqlite};

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
    /// content items that reference each via `ItemValues` of the given types
    /// (port of C# `GetItemValues`).
    ///
    /// Scoped to the browse's `parent_id` (via [`InternalItemsQuery::ancestor_ids`])
    /// and `include_item_types`: the Movies "Genres" tab lists only genres carried
    /// by movies, the TV "Networks" tab only studios carried by items under the TV
    /// library, each with an in-scope item count — matching Jellyfin, which scopes
    /// its by-name aggregates to the query. Only values with an in-scope item (and
    /// a materialized by-name row) appear.
    async fn item_values_with_counts(
        &self,
        value_types: &[ItemValueType],
        filter: &InternalItemsQuery,
        include_content_types: &[String],
        exclude_content_types: &[String],
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        let type_ints: Vec<i64> = value_types
            .iter()
            .map(|t| i64::from(i32::from(*t)))
            .collect();
        // Content-item scoping: the browse's requested kinds plus any caller-forced
        // types (music genres come only from music items; plain genres exclude them).
        let mut content_type_names: Vec<String> = filter
            .include_item_types
            .iter()
            .filter_map(|k| stored_type_name(*k).map(str::to_owned))
            .collect();
        content_type_names.extend(include_content_types.iter().cloned());
        let ancestors: Vec<String> = filter.ancestor_ids.iter().map(Uuid::to_string).collect();

        // The value ids referenced by in-scope content items, with in-scope counts.
        let mut sql = String::from(
            r#"SELECT iv."ItemValueId", COUNT(DISTINCT ivm."ItemId")
               FROM "ItemValues" iv
               JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
               JOIN "BaseItems" bi ON bi."Id" = ivm."ItemId"
               WHERE iv."Type" IN ("#,
        );
        sql.push_str(&placeholders(type_ints.len()));
        sql.push(')');
        if !content_type_names.is_empty() {
            sql.push_str(r#" AND bi."Type" IN ("#);
            sql.push_str(&placeholders(content_type_names.len()));
            sql.push(')');
        }
        if !exclude_content_types.is_empty() {
            sql.push_str(r#" AND bi."Type" NOT IN ("#);
            sql.push_str(&placeholders(exclude_content_types.len()));
            sql.push(')');
        }
        if !ancestors.is_empty() {
            sql.push_str(
                r#" AND EXISTS (SELECT 1 FROM "AncestorIds" a WHERE a."ItemId" = bi."Id" AND a."ParentItemId" IN ("#,
            );
            sql.push_str(&placeholders(ancestors.len()));
            sql.push_str("))");
        }
        sql.push_str(r#" GROUP BY iv."ItemValueId""#);

        let mut query = sqlx::query_as::<_, (String, i64)>(&sql);
        for t in &type_ints {
            query = query.bind(*t);
        }
        for n in &content_type_names {
            query = query.bind(n.clone());
        }
        for n in exclude_content_types {
            query = query.bind(n.clone());
        }
        for a in &ancestors {
            query = query.bind(a.clone());
        }
        let counts: Vec<(String, i64)> = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        if counts.is_empty() {
            return Ok(QueryResult::default());
        }

        // Load the materialized by-name rows (id = ItemValueId) for those values.
        let ids: Vec<String> = counts.iter().map(|(id, _)| id.clone()).collect();
        let mut esql = String::from(r#"SELECT * FROM "BaseItems" WHERE "Id" IN ("#);
        esql.push_str(&placeholders(ids.len()));
        esql.push(')');
        let mut equery = sqlx::query_as::<_, BaseItemEntity>(&esql);
        for id in &ids {
            equery = equery.bind(id.clone());
        }
        let mut by_id: std::collections::HashMap<String, BaseItemEntity> = equery
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();

        let mut items: Vec<ItemWithCounts> = counts
            .into_iter()
            .filter_map(|(id, cnt)| {
                by_id.remove(&id).map(|item| ItemWithCounts {
                    item,
                    counts: hermit_model::dto::ItemCounts {
                        item_count: i32::try_from(cnt).unwrap_or(i32::MAX),
                        ..Default::default()
                    },
                })
            })
            .collect();
        items.sort_by(|a, b| a.item.name.cmp(&b.item.name));
        Ok(QueryResult::from_items(items))
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

/// Maps the `BaseItemImageInfos.ImageType` integer discriminant to the wire
/// [`ImageType`]. The discriminants are the fixed `ImageInfoImageType` values and
/// line up 1:1 with [`ImageType`]; an out-of-range value falls back to
/// [`ImageType::Primary`] (the C# default when parsing a legacy row).
fn image_type_from_disc(disc: i32) -> ImageType {
    match disc {
        1 => ImageType::Art,
        2 => ImageType::Backdrop,
        3 => ImageType::Banner,
        4 => ImageType::Logo,
        5 => ImageType::Thumb,
        6 => ImageType::Disc,
        7 => ImageType::Box,
        8 => ImageType::Screenshot,
        9 => ImageType::Menu,
        10 => ImageType::Chapter,
        11 => ImageType::BoxRear,
        12 => ImageType::Profile,
        _ => ImageType::Primary,
    }
}

/// Maps a wire [`ImageType`] back to its `BaseItemImageInfos.ImageType` integer
/// discriminant — the inverse of [`image_type_from_disc`].
///
/// The discriminants line up 1:1 with the C# `ImageType` declaration order.
pub(crate) fn image_type_to_disc(image_type: ImageType) -> i32 {
    match image_type {
        ImageType::Primary => 0,
        ImageType::Art => 1,
        ImageType::Backdrop => 2,
        ImageType::Banner => 3,
        ImageType::Logo => 4,
        ImageType::Thumb => 5,
        ImageType::Disc => 6,
        ImageType::Box => 7,
        ImageType::Screenshot => 8,
        ImageType::Menu => 9,
        ImageType::Chapter => 10,
        ImageType::BoxRear => 11,
        ImageType::Profile => 12,
    }
}

/// Projects a persisted [`BaseItemImageInfoEntity`] row into an
/// [`ItemImageInfo`].
///
/// The stored `Blurhash` is a UTF-8 byte blob; an empty blob (or one that is not
/// valid UTF-8) becomes [`None`]. A zero/negative width or height (the "unknown"
/// sentinel) is preserved as-is; the API layer nulls those out per Jellyfin.
fn image_info_from_row(row: BaseItemImageInfoEntity) -> ItemImageInfo {
    let blur_hash = row
        .blurhash
        .filter(|b| !b.is_empty())
        .and_then(|b| String::from_utf8(b).ok());
    ItemImageInfo {
        path: row.path,
        image_type: image_type_from_disc(row.image_type),
        date_modified: row.date_modified.unwrap_or_else(default_epoch),
        width: i32::try_from(row.width).unwrap_or(0),
        height: i32::try_from(row.height).unwrap_or(0),
        blur_hash,
    }
}

/// The Unix epoch as a UTC timestamp — the placeholder for a row with no stored
/// `DateModified` (C# leaves the `default(DateTime)`).
fn default_epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_else(Utc::now)
}

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

    async fn get_items_by_primary_version(
        &self,
        primary_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if primary_id.is_nil() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT * FROM "BaseItems" WHERE "PrimaryVersionId" = ?1 AND "Id" <> ?2"#,
        )
        .bind(primary_id.to_string())
        .bind(PLACEHOLDER_ID)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn get_items_with_provider_id(
        &self,
        provider_key: &str,
    ) -> Result<Vec<(Uuid, String)>, ServiceError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "ItemId", "ProviderValue" FROM "BaseItemProviders"
               WHERE "ProviderId" = ?1 COLLATE NOCASE"#,
        )
        .bind(provider_key)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, value)| Uuid::parse_str(&id).ok().map(|id| (id, value)))
            .collect())
    }

    async fn get_image_infos(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError> {
        // Order by image type then id so a multi-image type (e.g. Backdrop) is
        // returned in a stable order the index-based routes can address, matching
        // the C# `BaseItem.ImageInfos` insertion order.
        let rows = sqlx::query_as::<_, BaseItemImageInfoEntity>(
            r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1
                ORDER BY "ImageType", "Id""#,
        )
        .bind(item_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(image_info_from_row).collect())
    }

    async fn swap_item_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<(), ServiceError> {
        // A same-index swap is a no-op (matching C#, where swapping a row with
        // itself changes nothing) and avoids a needless write.
        if index1 == index2 {
            return Ok(());
        }
        // Load this item's rows for the requested type in the same stable order
        // get_image_infos exposes, so the caller's 0-based indices address the
        // same images the read side does.
        let disc = image_type_to_disc(image_type);
        let rows = sqlx::query_as::<_, BaseItemImageInfoEntity>(
            r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2
                ORDER BY "Id""#,
        )
        .bind(item_id.to_string())
        .bind(disc)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        // Out-of-range indices are a no-op — the C# `GetImageInfo` returns null and
        // SwapImagesAsync bails with "nothing to do".
        let (Ok(i1), Ok(i2)) = (usize::try_from(index1), usize::try_from(index2)) else {
            return Ok(());
        };
        let (Some(first), Some(second)) = (rows.get(i1), rows.get(i2)) else {
            return Ok(());
        };

        // C# swaps the two on-disk files and clears the cached dimensions. The
        // portable equivalent over stored rows is to exchange the two rows' paths
        // (so the image previously at index1 now resolves at index2) and reset
        // Width/Height to the "unknown" sentinel, stamping DateModified.
        let now = Utc::now();
        let mut tx = self.db.pool().begin().await.map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "BaseItemImageInfos"
                SET "Path" = ?2, "Width" = 0, "Height" = 0, "DateModified" = ?3
                WHERE "Id" = ?1"#,
        )
        .bind(&first.id)
        .bind(&second.path)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "BaseItemImageInfos"
                SET "Path" = ?2, "Width" = 0, "Height" = 0, "DateModified" = ?3
                WHERE "Id" = ?1"#,
        )
        .bind(&second.id)
        .bind(&first.path)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn get_genres(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        // Plain genres exclude music items (those are the MusicGenres browse).
        let music = self.item_type_lookup.music_genre_types();
        self.item_values_with_counts(GENRE_TYPES, filter, &[], &music)
            .await
    }

    async fn get_music_genres(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        // Music genres come only from music items.
        let music = self.item_type_lookup.music_genre_types();
        self.item_values_with_counts(GENRE_TYPES, filter, &music, &[])
            .await
    }

    async fn get_studios(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(STUDIO_TYPES, filter, &[], &[])
            .await
    }

    async fn get_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(ARTIST_TYPES, filter, &[], &[])
            .await
    }

    async fn get_album_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(ALBUM_ARTIST_TYPES, filter, &[], &[])
            .await
    }

    async fn get_all_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(ALL_ARTIST_TYPES, filter, &[], &[])
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

    async fn get_media_stream_languages_by_type(
        &self,
        filter: &InternalItemsQuery,
        stream_types: &[MediaStreamType],
    ) -> Result<std::collections::HashMap<MediaStreamType, Vec<String>>, ServiceError> {
        let mut out: std::collections::HashMap<MediaStreamType, Vec<String>> =
            stream_types.iter().map(|&t| (t, Vec::new())).collect();
        // Resolve the item set once (the fetch_ids + IN was previously run per
        // type — audio and subtitle each re-materialized the same ids).
        let ids = self.fetch_ids(filter).await?;
        if ids.is_empty() || stream_types.is_empty() {
            return Ok(out);
        }
        // Map disc -> type so the grouped rows sort back into per-type lists.
        let by_disc: std::collections::HashMap<i64, MediaStreamType> = stream_types
            .iter()
            .map(|&t| (i64::from(media_stream_type_disc(t)), t))
            .collect();
        let mut sql = String::from(
            r#"SELECT DISTINCT ms."StreamType",
                 CASE WHEN ms."Language" IS NULL OR ms."Language" = '' THEN 'und'
                      ELSE ms."Language" END
               FROM "MediaStreamInfos" ms WHERE ms."StreamType" IN ("#,
        );
        sql.push_str(&placeholders(by_disc.len()));
        sql.push_str(r#") AND ms."ItemId" IN ("#);
        sql.push_str(&placeholders(ids.len()));
        sql.push(')');
        let mut query = sqlx::query_as::<_, (i64, String)>(&sql);
        for disc in by_disc.keys() {
            query = query.bind(*disc);
        }
        for id in &ids {
            query = query.bind(id.to_string());
        }
        for (disc, lang) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
            if let Some(t) = by_disc.get(&disc) {
                out.entry(*t).or_default().push(lang);
            }
        }
        Ok(out)
    }

    async fn get_query_filters_legacy(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        // Each facet runs the filter once as its own WHERE (via `append_predicates`)
        // instead of materializing the whole matching id set and binding it back as a
        // giant `IN` per facet. The old "resolve every matching id in the app, then
        // re-send them as a thousand-parameter IN, four times" round-trip dominated the
        // Filters/Filters2/Years CPU under load.
        let years = self.distinct_years(filter).await?;
        let official_ratings = self.distinct_official_ratings(filter).await?;
        let genres = self
            .distinct_item_values(filter, ItemValueType::Genre)
            .await?;
        let tags = self
            .distinct_item_values(filter, ItemValueType::Tags)
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
    /// Distinct positive production years of the filter's matching items, ascending —
    /// the filter runs as this query's own WHERE, no app-side id materialization.
    async fn distinct_years(&self, filter: &InternalItemsQuery) -> Result<Vec<i32>, ServiceError> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT DISTINCT bi."ProductionYear" FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        );
        qb.push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
        qb.push(
            r#" AND bi."ProductionYear" IS NOT NULL AND bi."ProductionYear" > 0
                ORDER BY bi."ProductionYear""#,
        );
        let rows: Vec<i64> = qb
            .build_query_scalar()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|y| i32::try_from(y).unwrap_or(i32::MAX))
            .collect())
    }

    /// Distinct non-empty official ratings of the filter's matching items, ascending.
    async fn distinct_official_ratings(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT DISTINCT bi."OfficialRating" FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        );
        qb.push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
        qb.push(
            r#" AND bi."OfficialRating" IS NOT NULL AND bi."OfficialRating" <> ''
                ORDER BY bi."OfficialRating""#,
        );
        qb.build_query_scalar()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// Distinct display values of one `ItemValues` type over the filter's matching items.
    async fn distinct_item_values(
        &self,
        filter: &InternalItemsQuery,
        value_type: ItemValueType,
    ) -> Result<Vec<String>, ServiceError> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT DISTINCT iv."Value" FROM "ItemValues" AS iv
               JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
               JOIN "BaseItems" AS bi ON bi."Id" = ivm."ItemId"
               WHERE iv."Type" = "#,
        );
        qb.push_bind(i64::from(i32::from(value_type)));
        qb.push(r#" AND bi."Id" <> "#);
        qb.push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
        qb.push(r#" ORDER BY iv."Value""#);
        qb.build_query_scalar()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
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
    use hermit_model::data::BaseItemKind;
    use hermit_model::entities::ExtraType;

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
    async fn recursive_parent_matches_descendants_via_ancestor_closure() {
        let db = test_db().await;
        let repository = repo(&db);
        // library ─ series ─ episode. The episode is a direct child of the series,
        // NOT of the library, but the library is in its ancestor closure.
        let library = Uuid::from_u128(0xB001);
        let series = Uuid::from_u128(0xB002);
        let episode = Uuid::from_u128(0xB003);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "TV").await;
        seed_named_item(&db, series, BaseItemKind::Series, "Show").await;
        seed_named_item(&db, episode, BaseItemKind::Episode, "Pilot").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(series.to_string())
            .bind(library.to_string())
            .execute(db.pool())
            .await
            .expect("series parent");
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(episode.to_string())
            .bind(series.to_string())
            .execute(db.pool())
            .await
            .expect("episode parent");
        for ancestor in [series, library] {
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(episode.to_string())
                .bind(ancestor.to_string())
                .execute(db.pool())
                .await
                .expect("ancestor");
        }

        // Non-recursive: the library has one direct child (the series), no episode.
        let direct = InternalItemsQuery {
            parent_id: library,
            include_item_types: vec![BaseItemKind::Episode],
            ..InternalItemsQuery::default()
        };
        assert!(
            repository
                .get_item_list(&direct)
                .await
                .expect("direct")
                .is_empty()
        );

        // Recursive: the episode is reached through the ancestor closure.
        let recursive = InternalItemsQuery {
            parent_id: library,
            recursive: true,
            include_item_types: vec![BaseItemKind::Episode],
            ..InternalItemsQuery::default()
        };
        let rows = repository
            .get_item_list(&recursive)
            .await
            .expect("recursive");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, episode.to_string());
    }

    #[tokio::test]
    async fn boxset_parent_browse_surfaces_linked_children() {
        use crate::linked_children_service::HermitLinkedChildrenService;
        use hermit_traits::persistence::LinkedChildrenService;

        let db = test_db().await;
        let repository = repo(&db);
        // A box-set and a movie. Membership lives ONLY as a LinkedChildren edge
        // (the movie's physical ParentId is unrelated), so a plain `ParentId`
        // browse must not see it — the merged `GetChildren` behaviour must.
        let boxset = Uuid::from_u128(0xB5E7);
        let movie = Uuid::from_u128(0xB5E8);
        seed_named_item(&db, boxset, BaseItemKind::BoxSet, "Trilogy").await;
        seed_named_item(&db, movie, BaseItemKind::Movie, "Part One").await;

        let links = HermitLinkedChildrenService::new(db.clone());
        // `add_to_collection` inserts a manual (ChildType = 0) edge.
        links
            .upsert_linked_child(boxset, movie, 0)
            .await
            .expect("add_to_collection");

        let query = InternalItemsQuery {
            parent_id: boxset,
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&query).await.expect("browse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, movie.to_string());

        // Removing the membership makes the browse empty again.
        sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1 AND "ChildId" = ?2"#)
            .bind(boxset.to_string())
            .bind(movie.to_string())
            .execute(db.pool())
            .await
            .expect("remove_from_collection");
        assert!(
            repository
                .get_item_list(&query)
                .await
                .expect("browse after remove")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn person_ids_filter_returns_that_persons_filmography() {
        let db = test_db().await;
        let repository = repo(&db);
        // Two movies; a person credited on only the first.
        let movie_a = Uuid::from_u128(0xC001);
        let movie_b = Uuid::from_u128(0xC002);
        let person = Uuid::from_u128(0xC0FF);
        seed_named_item(&db, movie_a, BaseItemKind::Movie, "Heat").await;
        seed_named_item(&db, movie_b, BaseItemKind::Movie, "Solaris").await;
        sqlx::query(
            r#"INSERT INTO "Peoples" ("Id","Name","PersonType") VALUES (?1,'Al Pacino','Actor')"#,
        )
        .bind(person.to_string())
        .execute(db.pool())
        .await
        .expect("person");
        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap" ("ItemId","PeopleId","Role","ListOrder","SortOrder")
               VALUES (?1,?2,'',0,0)"#,
        )
        .bind(movie_a.to_string())
        .bind(person.to_string())
        .execute(db.pool())
        .await
        .expect("credit");

        // By id: only the credited movie.
        let by_id = InternalItemsQuery {
            person_ids: vec![person],
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&by_id).await.expect("by id");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, movie_a.to_string());

        // By name resolves the same filmography.
        let by_name = InternalItemsQuery {
            person: Some("Al Pacino".to_owned()),
            ..InternalItemsQuery::default()
        };
        assert_eq!(
            repository
                .get_item_list(&by_name)
                .await
                .expect("by name")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn any_provider_id_equals_matches_exact_value_case_insensitively() {
        let db = test_db().await;
        let repository = repo(&db);

        // Two movies: Heat (Imdb tt0113277 + Tmdb 949) and Solaris (Tmdb 296).
        let heat = Uuid::from_u128(0xA001);
        let solaris = Uuid::from_u128(0xA002);
        seed_named_item(&db, heat, BaseItemKind::Movie, "Heat").await;
        seed_named_item(&db, solaris, BaseItemKind::Movie, "Solaris").await;
        for (item, provider, value) in [
            (heat, "Imdb", "tt0113277"),
            (heat, "Tmdb", "949"),
            (solaris, "Tmdb", "296"),
        ] {
            sqlx::query(
                r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
                   VALUES (?1, ?2, ?3)"#,
            )
            .bind(item.to_string())
            .bind(provider)
            .bind(value)
            .execute(db.pool())
            .await
            .expect("insert provider");
        }

        // Exact IMDb match (with a different-case value) selects only Heat.
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            any_provider_id_equals: vec![("imdb".to_owned(), "TT0113277".to_owned())],
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&query).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, heat.to_string());

        // A non-matching value returns nothing (no partial/prefix matching).
        let miss = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            any_provider_id_equals: vec![("Tmdb".to_owned(), "0".to_owned())],
            ..InternalItemsQuery::default()
        };
        assert!(
            repository
                .get_item_list(&miss)
                .await
                .expect("miss")
                .is_empty()
        );

        // Multiple pairs are OR-ed: Tmdb 296 OR Tmdb 949 selects both movies.
        let both = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            any_provider_id_equals: vec![
                ("Tmdb".to_owned(), "296".to_owned()),
                ("Tmdb".to_owned(), "949".to_owned()),
            ],
            ..InternalItemsQuery::default()
        };
        assert_eq!(
            repository.get_item_list(&both).await.expect("both").len(),
            2
        );
    }

    #[tokio::test]
    async fn get_image_infos_reads_rows_ordered_by_type() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9001);
        seed_named_item(&db, item, BaseItemKind::Movie, "Imaged").await;

        // A Backdrop (type 2) and a Primary (type 0); the query orders by type so
        // Primary comes back first regardless of insertion order.
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, ?2, NULL, 1080, 2, ?3, '/backdrop.jpg', 1920)"#,
        )
        .bind(Uuid::from_u128(0x9101).to_string())
        .bind("LKO2".as_bytes().to_vec())
        .bind(item.to_string())
        .execute(db.pool())
        .await
        .expect("insert backdrop");

        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, NULL, NULL, 0, 0, ?2, '/poster.jpg', 0)"#,
        )
        .bind(Uuid::from_u128(0x9102).to_string())
        .bind(item.to_string())
        .execute(db.pool())
        .await
        .expect("insert primary");

        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].image_type, ImageType::Primary);
        assert_eq!(images[0].path, "/poster.jpg");
        assert!(images[0].blur_hash.is_none());
        assert_eq!(images[1].image_type, ImageType::Backdrop);
        assert_eq!(images[1].path, "/backdrop.jpg");
        assert_eq!(images[1].width, 1920);
        assert_eq!(images[1].blur_hash.as_deref(), Some("LKO2"));

        // An item with no images yields an empty list.
        let none = repository
            .get_image_infos(Uuid::from_u128(0xDEAD))
            .await
            .expect("no images");
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn swap_item_images_reorders_two_backdrops() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9200);
        seed_named_item(&db, item, BaseItemKind::Movie, "Reorder Me").await;

        // Three backdrops (type 2), addressed by index 0/1/2 in Id order.
        for (n, path) in [(0u128, "/a.jpg"), (1, "/b.jpg"), (2, "/c.jpg")] {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                    ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                    VALUES (?1, NULL, NULL, 1080, 2, ?2, ?3, 1920)"#,
            )
            .bind(Uuid::from_u128(0x9210 + n).to_string())
            .bind(item.to_string())
            .bind(path)
            .execute(db.pool())
            .await
            .expect("insert backdrop");
        }

        // Swap index 0 (/a.jpg) with index 2 (/c.jpg).
        repository
            .swap_item_images(item, ImageType::Backdrop, 0, 2)
            .await
            .expect("swap");

        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images.len(), 3);
        // Paths are exchanged; the middle one is untouched. Dimensions of the two
        // swapped rows are reset to the unknown sentinel (0), matching C#.
        assert_eq!(images[0].path, "/c.jpg");
        assert_eq!(images[0].width, 0);
        assert_eq!(images[0].height, 0);
        assert_eq!(images[1].path, "/b.jpg");
        assert_eq!(images[1].width, 1920);
        assert_eq!(images[2].path, "/a.jpg");
        assert_eq!(images[2].width, 0);
    }

    #[tokio::test]
    async fn swap_item_images_out_of_range_index_is_noop() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9300);
        seed_named_item(&db, item, BaseItemKind::Movie, "One Backdrop").await;
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, NULL, NULL, 1080, 2, ?2, '/only.jpg', 1920)"#,
        )
        .bind(Uuid::from_u128(0x9310).to_string())
        .bind(item.to_string())
        .execute(db.pool())
        .await
        .expect("insert backdrop");

        // Index 5 does not exist — a faithful no-op, and the row is untouched.
        repository
            .swap_item_images(item, ImageType::Backdrop, 0, 5)
            .await
            .expect("noop swap");
        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].path, "/only.jpg");
        assert_eq!(images[0].width, 1920);
    }

    #[tokio::test]
    async fn genres_studios_artists_roll_up_with_counts() {
        let db = test_db().await;
        let repository = repo(&db);

        // Seeding a movie's genre also materializes the browsable by-name Genre
        // row (id = ItemValueId), the way the scanner does.
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

    #[tokio::test]
    async fn extra_types_filter_matches_stored_discriminant() {
        let db = test_db().await;
        let repository = repo(&db);

        // Two extras owned by a movie: one trailer, one behind-the-scenes.
        let owner = Uuid::from_u128(0xE000);
        seed_item(&db, owner, BaseItemKind::Movie).await;
        let trailer = Uuid::from_u128(0xE001);
        seed_named_item(&db, trailer, BaseItemKind::Trailer, "T").await;
        let behind = Uuid::from_u128(0xE002);
        seed_named_item(&db, behind, BaseItemKind::Video, "B").await;
        for (id, extra) in [
            (trailer, ExtraType::Trailer),
            (behind, ExtraType::BehindTheScenes),
        ] {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "ExtraType" = ?3 WHERE "Id" = ?1"#,
            )
            .bind(id.to_string())
            .bind(owner.to_string())
            .bind(extra as i32)
            .execute(db.pool())
            .await
            .expect("set extra");
        }

        // Filtering to Trailer extras owned by `owner` returns only the trailer.
        let query = InternalItemsQuery {
            owner_ids: vec![owner],
            extra_types: vec![ExtraType::Trailer],
            ..InternalItemsQuery::default()
        };
        let res = repository.get_item_list(&query).await.expect("extras");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, trailer.to_string());

        // Both display extras (Trailer + BehindTheScenes) return both.
        let query = InternalItemsQuery {
            owner_ids: vec![owner],
            extra_types: vec![ExtraType::Trailer, ExtraType::BehindTheScenes],
            ..InternalItemsQuery::default()
        };
        let res = repository.get_item_list(&query).await.expect("extras");
        assert_eq!(res.len(), 2);
    }

    #[tokio::test]
    async fn is_resumable_filters_on_in_progress_position() {
        let db = test_db().await;
        let repository = repo(&db);

        let user = seed_user(&db, Uuid::from_u128(0xF00D)).await;
        let resumable = Uuid::from_u128(0xF001);
        seed_item(&db, resumable, BaseItemKind::Movie).await;
        let not_resumable = Uuid::from_u128(0xF002);
        seed_item(&db, not_resumable, BaseItemKind::Movie).await;

        // A user-data row with a non-zero position marks the first item resumable.
        seed_user_data(&db, Uuid::from_u128(0xF00D), resumable, false, None).await;
        sqlx::query(r#"UPDATE "UserData" SET "PlaybackPositionTicks" = 5000 WHERE "ItemId" = ?1"#)
            .bind(resumable.to_string())
            .execute(db.pool())
            .await
            .expect("set position");

        let query = InternalItemsQuery {
            user: Some(user),
            is_resumable: Some(true),
            ..InternalItemsQuery::default()
        };
        let res = repository.get_item_list(&query).await.expect("resumable");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, resumable.to_string());
    }

    #[tokio::test]
    async fn get_items_by_primary_version_returns_alternates_only() {
        let db = test_db().await;
        let repository = repo(&db);
        let primary = Uuid::from_u128(0x0B01);
        let alt = Uuid::from_u128(0x0B02);
        let unrelated = Uuid::from_u128(0x0B03);
        seed_item(&db, primary, BaseItemKind::Movie).await;
        seed_item(&db, alt, BaseItemKind::Movie).await;
        seed_item(&db, unrelated, BaseItemKind::Movie).await;
        // Only `alt` points at `primary`.
        sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1 WHERE "Id" = ?2"#)
            .bind(primary.to_string())
            .bind(alt.to_string())
            .execute(db.pool())
            .await
            .expect("link alternate");

        let rows = repository
            .get_items_by_primary_version(primary)
            .await
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, alt.to_string());

        // A nil primary short-circuits to empty without hitting the pool.
        assert!(
            repository
                .get_items_by_primary_version(Uuid::nil())
                .await
                .expect("nil")
                .is_empty()
        );
    }
}
