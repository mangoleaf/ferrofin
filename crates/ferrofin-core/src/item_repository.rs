//! [`FerrofinItemRepository`] — the concrete [`ItemRepository`] over `ferrofin-db`.
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

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::{BaseItemEntity, BaseItemImageInfoEntity, ItemTextRow};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::ItemValueType;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::entities::ImageType;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_traits::options::ItemImageInfo;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{
    ItemRepository, ItemTypeLookup, ItemWithCounts, PlaylistAccessColumns, PlaylistItemsWithAccess,
};

use crate::db_error::{db_err, media_stream_type_disc};
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::{
    PLACEHOLDER_ID, QueryShape, append_predicates, build_query, push_in_list,
};
use sqlx::{FromRow, QueryBuilder, Row, Sqlite};

/// The concrete item repository.
///
/// Holds a cheaply-cloneable [`Database`] handle plus the shared
/// [`ItemTypeLookup`] (injected so the composition root can share one instance).
#[derive(Clone)]
pub struct FerrofinItemRepository {
    db: Database,
    item_type_lookup: Arc<dyn ItemTypeLookup>,
}

impl std::fmt::Debug for FerrofinItemRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinItemRepository")
            .finish_non_exhaustive()
    }
}

impl FerrofinItemRepository {
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

    /// Runs a translated query returning only the id and clean-name columns.
    ///
    /// Same statement as [`Self::fetch_rows`] but for the projection, so the
    /// rows — and their order — are identical to what `get_item_list` would
    /// have returned.
    async fn fetch_id_clean_names(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<(String, Option<String>)>, ServiceError> {
        let mut qb = build_query(filter, QueryShape::IdAndCleanName);
        qb.build_query_as::<(String, Option<String>)>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
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

    /// Fallback total for by-name pagination when the page is empty (past the
    /// last row). Accepts the SAME pre-computed vectors that the page query
    /// built, so the filter shape can never drift.
    async fn count_by_name_total(
        &self,
        type_ints: &[i64],
        content_type_names: &[String],
        exclude_content_types: &[String],
        ancestors: &[String],
        filter: &InternalItemsQuery,
    ) -> Result<i32, ServiceError> {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new(r#"SELECT COUNT(*) FROM "BaseItems" AS bi JOIN "#);
        push_value_aggregate(
            &mut qb,
            type_ints,
            content_type_names,
            exclude_content_types,
            ancestors,
        );
        qb.push(r#" ON agg.vid = bi."Id" WHERE 1 = 1"#);
        append_by_name_filters(&mut qb, filter);
        let count: i64 = qb
            .build_query_scalar()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
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
        let ancestors: Vec<String> = filter
            .ancestor_ids
            .iter()
            .copied()
            .map(guid_to_db)
            .collect();

        let want_total = filter.enable_total_record_count && filter.limit.is_some();
        let mut qb: QueryBuilder<Sqlite> = if want_total {
            QueryBuilder::new(
                r#"SELECT bi.*, agg.cnt, COUNT(*) OVER() AS "total_count" FROM "BaseItems" AS bi JOIN "#,
            )
        } else {
            QueryBuilder::new(r#"SELECT bi.*, agg.cnt FROM "BaseItems" AS bi JOIN "#)
        };
        push_value_aggregate(
            &mut qb,
            &type_ints,
            &content_type_names,
            exclude_content_types,
            &ancestors,
        );
        qb.push(r#" ON agg.vid = bi."Id" WHERE 1 = 1"#);
        append_by_name_filters(&mut qb, filter);
        // ORDER BY Name (the key the old in-memory sort used); parity harness is the
        // oracle for divergences from Jellyfin's SortName ordering.
        qb.push(r#" ORDER BY bi."Name" ASC"#);
        if let Some(limit) = filter.limit {
            qb.push(" LIMIT ").push_bind(i64::from(limit));
            let offset = filter.start_index.unwrap_or(0);
            if offset > 0 {
                qb.push(" OFFSET ").push_bind(i64::from(offset));
            }
        } else if let Some(offset) = filter.start_index.filter(|o| *o > 0) {
            qb.push(" LIMIT -1 OFFSET ").push_bind(i64::from(offset));
        }

        let rows = qb
            .build_query_as::<ByNameCountRow>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        // Total: C# `GetItemValues` forces EnableTotalRecordCount off when there is
        // no Limit and then never assigns `TotalRecordCount` — a non-nullable int —
        // so every unpaged (or count-disabled) by-name response carries
        // `TotalRecordCount: 0` on the wire, even with a bare StartIndex. Match
        // that exactly; the ledger's flagged /Genres//Studios read-diffs were this.
        let start_index = filter.start_index.unwrap_or(0);
        let total = if want_total {
            match rows.first() {
                Some(r) => i32::try_from(r.total_count).unwrap_or(i32::MAX),
                None if start_index > 0 => {
                    self.count_by_name_total(
                        &type_ints,
                        &content_type_names,
                        exclude_content_types,
                        &ancestors,
                        filter,
                    )
                    .await?
                }
                None => 0,
            }
        } else {
            0
        };
        let items: Vec<ItemWithCounts> = rows
            .into_iter()
            .map(|r| ItemWithCounts {
                item: r.item,
                counts: ferrofin_model::dto::ItemCounts {
                    item_count: i32::try_from(r.cnt).unwrap_or(i32::MAX),
                    ..Default::default()
                },
            })
            .collect();
        Ok(QueryResult::new(Some(start_index), Some(total), items))
    }
}

/// A by-name row plus its in-scope item count, read from the joined by-name
/// aggregate query (the `cnt` column is the aggregate's `COUNT(DISTINCT ItemId)`).
/// `total_count` carries the `COUNT(*) OVER()` window-function total when the
/// caller needs pagination metadata, avoiding a separate COUNT round-trip.
#[derive(sqlx::FromRow)]
struct ByNameCountRow {
    #[sqlx(flatten)]
    item: BaseItemEntity,
    cnt: i64,
    #[sqlx(default)]
    total_count: i64,
}

/// Pushes the value-count aggregate as a derived table `agg(vid, cnt)`: for each
/// `ItemValueId` of one of `type_ints`, the count of distinct in-scope content
/// items that reference it, scoped by content-type include/exclude and the
/// browse's `ancestors`. Shared by the page query and the total-count query so
/// their WHERE stays identical (C# `GetItemValues` inner filter).
fn push_value_aggregate<'a>(
    qb: &mut QueryBuilder<'a, Sqlite>,
    type_ints: &'a [i64],
    content_type_names: &'a [String],
    exclude_content_types: &'a [String],
    ancestors: &'a [String],
) {
    qb.push(
        r#"(SELECT iv."ItemValueId" AS vid, COUNT(DISTINCT ivm."ItemId") AS cnt
           FROM "ItemValues" iv
           JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
           JOIN "BaseItems" ci ON ci."Id" = ivm."ItemId"
           WHERE "#,
    );
    push_in_list(qb, r#"iv."Type""#, type_ints);
    if !content_type_names.is_empty() {
        qb.push(" AND ");
        push_in_list(qb, r#"ci."Type""#, content_type_names);
    }
    if !exclude_content_types.is_empty() {
        qb.push(r#" AND ci."Type" NOT IN ("#);
        let mut sep = qb.separated(", ");
        for n in exclude_content_types {
            sep.push_bind(n.clone());
        }
        qb.push(")");
    }
    if !ancestors.is_empty() {
        qb.push(r#" AND EXISTS (SELECT 1 FROM "AncestorIds" a WHERE a."ItemId" = ci."Id" AND "#);
        push_in_list(qb, r#"a."ParentItemId""#, ancestors);
        qb.push(")");
    }
    qb.push(r#" GROUP BY iv."ItemValueId") AS agg"#);
}

/// The characters C# `BaseItemRepository.SearchWildcardTerms` treats as "the
/// caller is doing a wildcard search" — their presence routes `SearchTerm` to a
/// raw (unescaped) `LIKE` instead of a literal contains-match.
const SEARCH_WILDCARD_TERMS: &[char] = &['%', '_', '[', ']', '^'];

/// Hard cap on the ancestor-chain CTE depth — prevents unbounded recursion on
/// a cyclic `ParentId` chain (real libraries are <10 levels deep).
const MAX_ANCESTOR_DEPTH: i32 = 32;

/// The one query behind `GET /Playlists/{id}/Items`: every member row of the
/// playlist, in link order, each carrying the caller's access columns.
///
/// Collapsing the read into a single statement is load-bearing under open-loop
/// load — the previous shape took one reader-pool connection for the access
/// check, another for the child-id list, and another for the `IN (…)` detail
/// fetch, so a single request queued three times on a pool sized to the core
/// count. `?1` = playlist id, `?2` = caller, `?3` = the linked-child type.
///
/// `pl` proves the playlist's own `BaseItems` row exists (a member edge without
/// its container must still read as missing, exactly as the access query does);
/// `p`/`s` are LEFT-joined so a legacy playlist with no meta row and a caller
/// with no share row both come back as NULLs the playlist manager interprets.
const PLAYLIST_ITEMS_SQL: &str = r#"SELECT ch.*,
       p."OwnerUserId" AS "PlaylistOwnerUserId",
       p."OpenAccess"  AS "PlaylistOpenAccess",
       s."CanEdit"     AS "PlaylistShareCanEdit"
   FROM "FerrofinLinkedChildren" lc
   JOIN "BaseItems" pl ON pl."Id" = lc."ParentId"
   JOIN "BaseItems" ch ON ch."Id" = lc."ChildId"
   LEFT JOIN "FerrofinPlaylists" p ON p."PlaylistId" = lc."ParentId"
   LEFT JOIN "FerrofinPlaylistShares" s
          ON s."PlaylistId" = lc."ParentId" AND s."UserId" = ?2
   WHERE lc."ParentId" = ?1 AND lc."ChildType" = ?3
   ORDER BY lc."SortOrder""#;

/// One row of [`PLAYLIST_ITEMS_SQL`]: a member's `BaseItems` row plus the three
/// access columns (identical on every row of a given playlist).
struct PlaylistItemRow {
    /// The member item row.
    item: BaseItemEntity,
    /// `FerrofinPlaylists.OwnerUserId` (NULL for a legacy or API-key playlist).
    owner: Option<String>,
    /// `FerrofinPlaylists.OpenAccess` (NULL only when the meta row is absent).
    open_access: Option<i64>,
    /// The caller's `FerrofinPlaylistShares.CanEdit` (NULL when not shared).
    can_edit: Option<i64>,
}

impl<'r> FromRow<'r, sqlx::sqlite::SqliteRow> for PlaylistItemRow {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            item: BaseItemEntity::from_row(row)?,
            owner: row.try_get("PlaylistOwnerUserId")?,
            open_access: row.try_get("PlaylistOpenAccess")?,
            can_edit: row.try_get("PlaylistShareCanEdit")?,
        })
    }
}

/// Appends the caller's name filters against the by-name `bi` row (C#
/// `TranslateQuery`'s name predicates on the outer query).
///
/// - `SearchTerm` ports the C# two-branch shape: a term containing a wildcard
///   character goes through `LIKE '%term%'` **unescaped** (client wildcards pass
///   through, as upstream intends); a plain term is a literal contains-match.
///   Deliberate divergence: the plain term is cleaned (`get_clean_value`) before
///   matching `CleanName` — C# compares the raw lowered term against the cleaned
///   column, which makes e.g. hyphenated searches miss; Ferrofin stays correct.
/// - `NameStartsWith` is a prefix on the sort key. Deliberate divergence:
///   `COALESCE(SortName, Name)` because Ferrofin leaves by-name `SortName` NULL
///   (C# filters `SortName` alone, which would match nothing here).
/// - `NameStartsWithOrGreater`/`NameLessThan` port C#'s **first-character**
///   comparison (`SortName.FirstOrDefault() > x[0] || Name.FirstOrDefault() >
///   x[0]`), not a full-string range: `substr(...,1,1)` on both columns, OR'd,
///   binary collation — byte-identical to the C# char compare for ASCII.
fn append_by_name_filters<'a>(qb: &mut QueryBuilder<'a, Sqlite>, filter: &'a InternalItemsQuery) {
    let non_blank = |v: &'a Option<String>| v.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // Favorite state lives in the by-name item's own `UserData` row (C# joins
    // `UserData` on the by-name item id) — same predicate the main browse uses.
    if let (Some(user_id), Some(want)) = (filter.user_id(), filter.is_favorite) {
        crate::translate_query::push_user_data_exists(
            qb,
            &guid_to_db(user_id),
            r#"ud."IsFavorite" = 1"#,
            want,
        );
    }
    if let Some(term) = non_blank(&filter.search_term) {
        let lowered = term.to_lowercase();
        if lowered.contains(SEARCH_WILDCARD_TERMS) {
            let like = format!("%{}%", lowered.trim_matches('%'));
            qb.push(r#" AND lower(bi."CleanName") LIKE "#)
                .push_bind(like);
        } else {
            let like = format!(
                "%{}%",
                crate::text_util::get_clean_value(term).trim_matches('%')
            );
            qb.push(r#" AND bi."CleanName" LIKE "#).push_bind(like);
        }
    }
    if let Some(prefix) = non_blank(&filter.name_starts_with) {
        qb.push(r#" AND lower(COALESCE(bi."SortName", bi."Name")) LIKE "#)
            .push_bind(format!("{}%", prefix.to_lowercase()));
    }
    if let Some(first) = non_blank(&filter.name_starts_with_or_greater)
        .and_then(|b| b.chars().next())
        .map(String::from)
    {
        qb.push(r#" AND (substr(bi."SortName", 1, 1) > "#)
            .push_bind(first.clone());
        qb.push(r#" OR substr(bi."Name", 1, 1) > "#)
            .push_bind(first);
        qb.push(")");
    }
    if let Some(first) = non_blank(&filter.name_less_than)
        .and_then(|b| b.chars().next())
        .map(String::from)
    {
        qb.push(r#" AND (substr(bi."SortName", 1, 1) < "#)
            .push_bind(first.clone());
        qb.push(r#" OR substr(bi."Name", 1, 1) < "#)
            .push_bind(first);
        qb.push(")");
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
impl ItemRepository for FerrofinItemRepository {
    async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id.is_nil() {
            return Err(ServiceError::invalid_input("item id can't be empty"));
        }
        let row = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1 AND "Id" <> ?2"#,
        )
        .bind(guid_to_db(id))
        .bind(PLACEHOLDER_ID)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row)
    }

    async fn locked_item_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        let rows: Vec<String> =
            sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "IsLocked" = 1"#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect())
    }

    async fn item_text_rows(
        &self,
        kind: BaseItemKind,
        ids: &[Uuid],
    ) -> Result<Vec<ItemTextRow>, ServiceError> {
        let Some(type_name) = stored_type_name(kind) else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        // 500 stays far below SQLite's conservative 999-host-variable floor.
        for chunk in ids.chunks(500) {
            // The anonymous `?` list must come FIRST: SQLite gives an
            // anonymous parameter the next index after the largest assigned so
            // far, so an explicit `?N` ahead of the list pushes every `?` in it
            // past the bound arguments and the query silently matches nothing.
            let sql = format!(
                r#"SELECT "Id", "Name", "SortName", "Overview", "Path"
                   FROM "BaseItems" WHERE "Id" IN ({}) AND "Type" = ?{}"#,
                placeholders(chunk.len()),
                chunk.len() + 1
            );
            let mut query = sqlx::query_as::<_, ItemTextRow>(&sql);
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            query = query.bind(type_name);
            rows.extend(query.fetch_all(self.db.pool()).await.map_err(db_err)?);
        }
        Ok(rows)
    }

    async fn get_ancestor_chain(
        &self,
        item_id: Uuid,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
        let db_id = guid_to_db(item_id);
        let rows: Vec<BaseItemEntity> = sqlx::query_as(
            r#"WITH RECURSIVE chain(id, depth) AS (
                 SELECT ?1, 0
                 UNION ALL
                 SELECT bi."ParentId", c.depth + 1
                 FROM chain c
                 JOIN "BaseItems" bi ON bi."Id" = c.id
                 WHERE bi."ParentId" IS NOT NULL
                   AND bi."ParentId" <> ?2
                   AND c.depth < ?3
               )
               SELECT bi.* FROM chain c
               JOIN "BaseItems" bi ON bi."Id" = c.id
               WHERE c.depth > 0
               ORDER BY c.depth ASC"#,
        )
        .bind(&db_id)
        .bind(PLACEHOLDER_ID)
        .bind(MAX_ANCESTOR_DEPTH)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        if rows.is_empty() {
            let exists = self.retrieve_item(item_id).await?.is_some();
            return if exists {
                Ok(Some(Vec::new()))
            } else {
                Ok(None)
            };
        }
        // Deduplicate by id (nearest-first) in case the tree has a cycle.
        let mut seen = std::collections::HashSet::new();
        let deduped = rows
            .into_iter()
            .filter(|r| seen.insert(r.id.clone()))
            .collect();
        Ok(Some(deduped))
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

    async fn get_item_id_clean_names(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<(String, Option<String>)>, ServiceError> {
        self.fetch_id_clean_names(filter).await
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
            ferrofin_model::live_tv::ItemSortBy::DateCreated,
            ferrofin_model::dto::SortOrder::Descending,
        )];
        self.fetch_rows(&latest, QueryShape::FullRows).await
    }

    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        let exists: Option<i64> =
            sqlx::query_scalar(r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
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
        .bind(guid_to_db(primary_id))
        .bind(PLACEHOLDER_ID)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn get_items_by_primary_version_batch(
        &self,
        primary_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
        let mut map: HashMap<Uuid, Vec<BaseItemEntity>> = HashMap::new();
        // 500 stays far below SQLite's conservative 999-host-variable floor.
        for chunk in primary_ids.chunks(500) {
            let sql = format!(
                r#"SELECT * FROM "BaseItems"
                   WHERE "PrimaryVersionId" IN ({}) AND "Id" <> ?{}"#,
                placeholders(chunk.len()),
                chunk.len() + 1
            );
            let mut query = sqlx::query_as::<_, BaseItemEntity>(&sql);
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            query = query.bind(PLACEHOLDER_ID);
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Some(primary) = row
                    .primary_version_id
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    map.entry(primary).or_default().push(row);
                }
            }
        }
        Ok(map)
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
        .bind(guid_to_db(item_id))
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
        .bind(guid_to_db(item_id))
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
        let now = datetime_to_db(Utc::now());
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "BaseItemImageInfos"
                SET "Path" = ?2, "Width" = 0, "Height" = 0, "DateModified" = ?3
                WHERE "Id" = ?1"#,
        )
        .bind(&first.id)
        .bind(&second.path)
        .bind(&now)
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
        .bind(&now)
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
            query = query.bind(guid_to_db(*id));
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
            query = query.bind(guid_to_db(*id));
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
            .bind(guid_to_db(id))
            .bind(uid)
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(all_played != 0)
    }

    async fn get_playlist_items_with_access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
        child_type: i32,
    ) -> Result<PlaylistItemsWithAccess, ServiceError> {
        let rows: Vec<PlaylistItemRow> = sqlx::query_as(PLAYLIST_ITEMS_SQL)
            .bind(guid_to_db(playlist_id))
            .bind(guid_to_db(user_id))
            .bind(i64::from(child_type))
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        // Every row repeats the same access columns; the first carries them.
        // No rows at all means the join matched nothing — the caller decides
        // whether that is an empty playlist or a missing/invisible one.
        let access = rows.first().map(|first| PlaylistAccessColumns {
            owner_user_id: first.owner.clone(),
            open_access: first.open_access,
            share_can_edit: first.can_edit,
        });
        Ok(PlaylistItemsWithAccess {
            items: rows.into_iter().map(|r| r.item).collect(),
            access,
        })
    }
}

impl FerrofinItemRepository {
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
        // One entry per CLEANED value (upstream GetQueryFiltersLegacy groups by
        // CleanValue and keeps MIN(Value)), so "Sci-Fi"/"Sci-fi" case variants
        // collapse instead of doubling the filter dialog's list.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT MIN(iv."Value") FROM "ItemValues" AS iv
               JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
               JOIN "BaseItems" AS bi ON bi."Id" = ivm."ItemId"
               WHERE iv."Type" = "#,
        );
        qb.push_bind(i64::from(i32::from(value_type)));
        qb.push(r#" AND bi."Id" <> "#);
        qb.push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
        qb.push(r#" GROUP BY iv."CleanValue" ORDER BY MIN(iv."Value")"#);
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
        seed_item, seed_item_genre, seed_named_item, seed_user, seed_user_data, set_clean_name,
        test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::ExtraType;

    fn repo(db: &Database) -> FerrofinItemRepository {
        FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
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

    /// The locked-item read must SEEK the partial index, never scan `BaseItems`.
    ///
    /// `run_scan` reads this set once, and it is shared with `scan_paths` —
    /// the library-monitor path that runs for one or two items on every
    /// filesystem event. Without `FerrofinIX_BaseItems_IsLocked` (migration
    /// 0017) the plan is `SCAN BaseItems`: 26-53 ms warm on a 100k-item table,
    /// paid per watcher event, where the old per-item lookup cost ~1 ms.
    #[tokio::test]
    async fn locked_item_ids_seeks_the_partial_index() {
        let db = test_db().await;
        let plan: Vec<String> = sqlx::query_as::<_, (i64, i64, i64, String)>(
            r#"EXPLAIN QUERY PLAN SELECT "Id" FROM "BaseItems" WHERE "IsLocked" = 1"#,
        )
        .fetch_all(db.pool())
        .await
        .expect("explain query plan")
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect();
        assert!(
            plan.iter()
                .any(|d| d.contains("FerrofinIX_BaseItems_IsLocked")),
            "the locked-item read must use the partial index, got: {plan:?}"
        );
        assert!(
            !plan.iter().any(|d| d.trim() == "SCAN BaseItems"),
            "the locked-item read must not scan BaseItems, got: {plan:?}"
        );
    }

    /// Two rows sharing a `CleanName` must resolve to the SAME id under both
    /// projections — the tie-break the by-name resolvers depend on.
    ///
    /// This is the one place the narrower projection could legitimately change
    /// the answer. `get_named_item_ids` passes a default query, so the ORDER BY
    /// is the bare `SortName`, and Person rows in a real database have
    /// `SortName IS NULL` — a total tie, where row order among duplicates is
    /// whatever the sorter happens to emit. The resolvers keep the first match
    /// (`or_insert`), so the two forms must agree on which row comes first.
    ///
    /// It is currently latent rather than live: `BaseItems.Id` is a TEXT PRIMARY
    /// KEY on a rowid table, so no index covers either projection and
    /// EXPLAIN QUERY PLAN is byte-identical for both. This test is what would
    /// catch that changing.
    #[tokio::test]
    async fn duplicate_clean_names_resolve_identically_under_both_projections() {
        let db = test_db().await;
        let repository = repo(&db);
        // Same CleanName, no SortName — the total-tie case.
        for n in [0x9201u128, 0x9202, 0x9203] {
            seed_named_item(&db, Uuid::from_u128(n), BaseItemKind::Person, "Jane Doe").await;
            set_clean_name(&db, Uuid::from_u128(n), "Jane Doe").await;
            sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = NULL WHERE "Id" = ?1"#)
                .bind(guid_to_db(Uuid::from_u128(n)))
                .execute(db.writer())
                .await
                .expect("null sort name");
        }

        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Person],
            ..InternalItemsQuery::default()
        };

        let first_full = |rows: Vec<ferrofin_db::entities::base_items::BaseItemEntity>| {
            rows.first().map(|r| r.id.clone())
        };
        // Repeated, because a tie-break that is merely *usually* stable is not a
        // tie-break — the resolvers cache the first id they see.
        for round in 0..5 {
            let full = first_full(repository.get_item_list(&query).await.expect("rows"));
            let pairs = repository
                .get_item_id_clean_names(&query)
                .await
                .expect("pairs");
            assert_eq!(
                pairs.len(),
                3,
                "round {round}: both forms must return every duplicate"
            );
            assert_eq!(
                full,
                pairs.first().map(|(id, _)| id.clone()),
                "round {round}: the two projections disagree on which duplicate is first, \
                 so the by-name resolvers would resolve different ids"
            );
        }
    }

    // The two-column projection is only safe because it runs the SAME statement
    // as `get_item_list` — same predicates, same ordering, same paging — so its
    // rows must line up with the full-row form one for one. If the two ever
    // drift, the by-name resolvers silently resolve the wrong id.
    #[tokio::test]
    async fn id_clean_name_projection_matches_the_full_row_query() {
        let db = test_db().await;
        let repository = repo(&db);
        for (n, name) in [(0x9101, "Zulu"), (0x9102, "Alpha"), (0x9103, "Mike")] {
            seed_named_item(&db, Uuid::from_u128(n), BaseItemKind::Movie, name).await;
            // The clean name is the join key the by-name resolvers match on, so
            // the projection has to carry the CLEANED value, not the display name.
            set_clean_name(&db, Uuid::from_u128(n), name).await;
            // A real SortName, so the shared ORDER BY actually orders and the
            // two forms are compared on a non-trivial row order.
            sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(Uuid::from_u128(n)))
                .bind(name.to_lowercase())
                .execute(db.writer())
                .await
                .expect("sort name");
        }
        // A different kind must be excluded by both forms alike.
        seed_named_item(&db, Uuid::from_u128(0x9104), BaseItemKind::Series, "Alpha").await;

        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::SortName,
                ferrofin_model::dto::SortOrder::Ascending,
            )],
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&query).await.expect("rows");
        let pairs = repository
            .get_item_id_clean_names(&query)
            .await
            .expect("pairs");
        assert_eq!(pairs.len(), 3, "the Series row must not leak in");
        assert_eq!(
            pairs.iter().map(|(_, c)| c.clone()).collect::<Vec<_>>(),
            vec![
                Some("alpha".to_owned()),
                Some("mike".to_owned()),
                Some("zulu".to_owned())
            ],
            "the projection carries the CLEAN name, in the query's sort order"
        );
        let expected: Vec<(String, Option<String>)> =
            rows.into_iter().map(|r| (r.id, r.clean_name)).collect();
        assert_eq!(pairs, expected);

        // Descending is the order no unordered scan can produce by accident, so
        // this is what pins the shared ORDER BY rather than a lucky row order.
        let desc = InternalItemsQuery {
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::SortName,
                ferrofin_model::dto::SortOrder::Descending,
            )],
            ..query.clone()
        };
        assert_eq!(
            repository
                .get_item_id_clean_names(&desc)
                .await
                .expect("desc pairs")
                .iter()
                .map(|(_, c)| c.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("zulu".to_owned()),
                Some("mike".to_owned()),
                Some("alpha".to_owned())
            ]
        );

        // Paging applies to the projection exactly as it does to the rows.
        let paged = InternalItemsQuery {
            limit: Some(2),
            start_index: Some(1),
            ..query.clone()
        };
        let paged_rows: Vec<(String, Option<String>)> = repository
            .get_item_list(&paged)
            .await
            .expect("paged rows")
            .into_iter()
            .map(|r| (r.id, r.clean_name))
            .collect();
        assert_eq!(paged_rows.len(), 2);
        assert_eq!(
            repository
                .get_item_id_clean_names(&paged)
                .await
                .expect("paged pairs"),
            paged_rows
        );
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
            .bind(guid_to_db(series))
            .bind(guid_to_db(library))
            .execute(db.writer())
            .await
            .expect("series parent");
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(episode))
            .bind(guid_to_db(series))
            .execute(db.writer())
            .await
            .expect("episode parent");
        for ancestor in [series, library] {
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(guid_to_db(episode))
                .bind(guid_to_db(ancestor))
                .execute(db.writer())
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
        assert_eq!(rows[0].id, guid_to_db(episode));
    }

    #[tokio::test]
    async fn boxset_parent_browse_surfaces_linked_children() {
        use crate::linked_children_service::FerrofinLinkedChildrenService;
        use ferrofin_traits::persistence::LinkedChildrenService;

        let db = test_db().await;
        let repository = repo(&db);
        // A box-set and a movie. Membership lives ONLY as a LinkedChildren edge
        // (the movie's physical ParentId is unrelated), so a plain `ParentId`
        // browse must not see it — the merged `GetChildren` behaviour must.
        let boxset = Uuid::from_u128(0xB5E7);
        let movie = Uuid::from_u128(0xB5E8);
        seed_named_item(&db, boxset, BaseItemKind::BoxSet, "Trilogy").await;
        seed_named_item(&db, movie, BaseItemKind::Movie, "Part One").await;

        let links = FerrofinLinkedChildrenService::new(db.clone());
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
        assert_eq!(rows[0].id, guid_to_db(movie));

        // Removing the membership makes the browse empty again.
        sqlx::query(
            r#"DELETE FROM "FerrofinLinkedChildren" WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
        )
        .bind(guid_to_db(boxset))
        .bind(guid_to_db(movie))
        .execute(db.writer())
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
        .bind(guid_to_db(person))
        .execute(db.writer())
        .await
        .expect("person");
        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap" ("ItemId","PeopleId","Role","ListOrder","SortOrder")
               VALUES (?1,?2,'',0,0)"#,
        )
        .bind(guid_to_db(movie_a))
        .bind(guid_to_db(person))
        .execute(db.writer())
        .await
        .expect("credit");

        // By id: only the credited movie.
        let by_id = InternalItemsQuery {
            person_ids: vec![person],
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&by_id).await.expect("by id");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, guid_to_db(movie_a));

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

        // Production grain: the browsable `Person` item has a DIFFERENT id from
        // the `Peoples` row (per-name deterministic vs per-(name,type) row id).
        // The client opens the person by its item id — the filmography must still
        // resolve via the name bridge, not just when the two ids coincide.
        let person_item = Uuid::from_u128(0xB0FF);
        assert_ne!(person_item, person);
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id","Type","Name","IsFolder","IsInMixedFolder","IsLocked",
                "IsMovie","IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1,'MediaBrowser.Controller.Entities.Person','Al Pacino',
                       0,0,0,0,0,0,0)"#,
        )
        .bind(guid_to_db(person_item))
        .execute(db.writer())
        .await
        .expect("person item");
        let by_item_id = InternalItemsQuery {
            person_ids: vec![person_item],
            ..InternalItemsQuery::default()
        };
        let rows = repository
            .get_item_list(&by_item_id)
            .await
            .expect("by item id");
        assert_eq!(
            rows.len(),
            1,
            "person item id resolves filmography via name"
        );
        assert_eq!(rows[0].id, guid_to_db(movie_a));
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
            .bind(guid_to_db(item))
            .bind(provider)
            .bind(value)
            .execute(db.writer())
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
        assert_eq!(rows[0].id, guid_to_db(heat));

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
        .bind(guid_to_db(Uuid::from_u128(0x9101)))
        .bind("LKO2".as_bytes().to_vec())
        .bind(guid_to_db(item))
        .execute(db.writer())
        .await
        .expect("insert backdrop");

        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, NULL, NULL, 0, 0, ?2, '/poster.jpg', 0)"#,
        )
        .bind(guid_to_db(Uuid::from_u128(0x9102)))
        .bind(guid_to_db(item))
        .execute(db.writer())
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
            .bind(guid_to_db(Uuid::from_u128(0x9210 + n)))
            .bind(guid_to_db(item))
            .bind(path)
            .execute(db.writer())
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
        .bind(guid_to_db(Uuid::from_u128(0x9310)))
        .bind(guid_to_db(item))
        .execute(db.writer())
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
    async fn by_name_paging_and_filters_push_into_sql() {
        let db = test_db().await;
        let repository = repo(&db);

        // Five distinct genres, ordered by Name: Action, Adventure, Comedy, Drama,
        // Horror. A second movie shares "Drama" so its in-scope count is 2.
        let movie = Uuid::from_u128(0xA001);
        seed_named_item(&db, movie, BaseItemKind::Movie, "One").await;
        for g in ["Action", "Adventure", "Comedy", "Drama", "Horror"] {
            seed_item_genre(&db, movie, g).await;
        }
        let movie2 = Uuid::from_u128(0xA002);
        seed_named_item(&db, movie2, BaseItemKind::Movie, "Two").await;
        seed_item_genre(&db, movie2, "Drama").await;

        // Paging: offset 1, limit 2 → [Adventure, Comedy], total = all 5.
        let page = InternalItemsQuery {
            start_index: Some(1),
            limit: Some(2),
            ..Default::default()
        };
        let paged = repository.get_genres(&page).await.expect("paged");
        let names: Vec<_> = paged
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(names, vec!["Adventure", "Comedy"]);
        assert_eq!(paged.start_index, 1);
        assert_eq!(paged.total_record_count, 5);

        // nameStartsWith is a prefix filter on the by-name row. Unpaged responses
        // carry TotalRecordCount 0 — C# forces EnableTotalRecordCount off without
        // a Limit and the non-nullable int is never assigned.
        let starts = InternalItemsQuery {
            name_starts_with: Some("A".to_owned()),
            ..Default::default()
        };
        let a = repository.get_genres(&starts).await.expect("starts");
        let a_names: Vec<_> = a.items.iter().filter_map(|i| i.item.name.clone()).collect();
        assert_eq!(a_names, vec!["Action", "Adventure"]);
        assert_eq!(a.total_record_count, 0);

        // nameLessThan compares only the FIRST character (C# `FirstOrDefault() <`):
        // "Comedy" itself is excluded ('C' < 'C' is false)…
        let less = InternalItemsQuery {
            name_less_than: Some("Comedy".to_owned()),
            ..Default::default()
        };
        let l = repository.get_genres(&less).await.expect("less");
        let l_names: Vec<_> = l.items.iter().filter_map(|i| i.item.name.clone()).collect();
        assert_eq!(l_names, vec!["Action", "Adventure"]);
        // …and so is everything else starting with 'C', even when the full string
        // sorts below the bound ("Comedy" < "Cz" as strings, but 'C' < 'C' fails).
        let less_cz = InternalItemsQuery {
            name_less_than: Some("Cz".to_owned()),
            ..Default::default()
        };
        let lcz = repository.get_genres(&less_cz).await.expect("less cz");
        let lcz_names: Vec<_> = lcz
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(lcz_names, vec!["Action", "Adventure"]);

        // A plain searchTerm is a literal contains-match on CleanName, and the
        // count survives it.
        let search = InternalItemsQuery {
            search_term: Some("ram".to_owned()),
            ..Default::default()
        };
        let s = repository.get_genres(&search).await.expect("search");
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.items[0].item.name.as_deref(), Some("Drama"));
        assert_eq!(s.items[0].counts.item_count, 2);

        // A searchTerm carrying a wildcard character routes through raw LIKE —
        // the `_` matches any one character (C# SearchWildcardTerms branch).
        let wild = InternalItemsQuery {
            search_term: Some("dr_ma".to_owned()),
            ..Default::default()
        };
        let w = repository.get_genres(&wild).await.expect("wild");
        assert_eq!(w.items.len(), 1);
        assert_eq!(w.items[0].item.name.as_deref(), Some("Drama"));
    }

    #[tokio::test]
    async fn by_name_favorite_filter_matches_the_genre_rows_user_data() {
        let db = test_db().await;
        let repository = repo(&db);

        // Two movies carrying one genre each; seeding materializes the browsable
        // by-name Genre rows (id = ItemValueId).
        let action_movie = Uuid::from_u128(0xFA01);
        seed_named_item(&db, action_movie, BaseItemKind::Movie, "Action Film").await;
        seed_item_genre(&db, action_movie, "Action").await;
        let drama_movie = Uuid::from_u128(0xFA02);
        seed_named_item(&db, drama_movie, BaseItemKind::Movie, "Drama Film").await;
        seed_item_genre(&db, drama_movie, "Drama").await;

        let user_id = Uuid::from_u128(0xFA10);
        let user = seed_user(&db, user_id).await;

        // Favorite the "Action" genre: the state lives in the by-name row's OWN
        // UserData (C# joins UserData on the by-name item id), so the row is
        // keyed to the materialized Genre item, not to either movie.
        let genre_id: String = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems"
               WHERE "Name" = 'Action'
                 AND "Type" = 'MediaBrowser.Controller.Entities.Genre'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("materialized genre row");
        sqlx::query(
            r#"INSERT INTO "UserData"
               ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
                "PlaybackPositionTicks", "Played")
               VALUES (?1, ?2, ?1, 1, 0, 0, 0)"#,
        )
        .bind(&genre_id)
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("favorite the genre");

        // isFavorite=true keeps only the favorited genre…
        let fav = InternalItemsQuery {
            user: Some(user.clone()),
            is_favorite: Some(true),
            ..Default::default()
        };
        let got = repository.get_genres(&fav).await.expect("favorites");
        let names: Vec<_> = got
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(names, vec!["Action"]);

        // …and isFavorite=false keeps only the un-favorited one (NOT EXISTS).
        let not_fav = InternalItemsQuery {
            user: Some(user),
            is_favorite: Some(false),
            ..Default::default()
        };
        let got = repository
            .get_genres(&not_fav)
            .await
            .expect("non-favorites");
        let names: Vec<_> = got
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(names, vec!["Drama"]);
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
        .bind(guid_to_db(movie))
        .execute(db.writer())
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
                    "StreamType", "Language")
                   VALUES (?1, ?2, 0, 0, 0, 0, ?3)"#,
            )
            .bind(guid_to_db(item))
            .bind(idx)
            .bind(lang)
            .execute(db.writer())
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
            .bind(guid_to_db(child))
            .bind(guid_to_db(parent))
            .execute(db.writer())
            .await
            .expect("set parent");

        let user = seed_user(&db, Uuid::from_u128(0xC0DE)).await;
        seed_user_data(&db, Uuid::from_u128(0xC0DE), child, true, None).await;

        // The recursive branch runs its AncestorIds join. With an ancestor row
        // present, the played child makes the closure fully played.
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(child))
            .bind(guid_to_db(parent))
            .execute(db.writer())
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
            .bind(guid_to_db(unplayed))
            .bind(guid_to_db(parent))
            .execute(db.writer())
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
            .bind(guid_to_db(id))
            .bind(guid_to_db(owner))
            .bind(extra as i32)
            .execute(db.writer())
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
        assert_eq!(res[0].id, guid_to_db(trailer));

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
            .bind(guid_to_db(resumable))
            .execute(db.writer())
            .await
            .expect("set position");

        let query = InternalItemsQuery {
            user: Some(user),
            is_resumable: Some(true),
            ..InternalItemsQuery::default()
        };
        let res = repository.get_item_list(&query).await.expect("resumable");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, guid_to_db(resumable));
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
            .bind(guid_to_db(primary))
            .bind(guid_to_db(alt))
            .execute(db.writer())
            .await
            .expect("link alternate");

        let rows = repository
            .get_items_by_primary_version(primary)
            .await
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, guid_to_db(alt));

        // A nil primary short-circuits to empty without hitting the pool.
        assert!(
            repository
                .get_items_by_primary_version(Uuid::nil())
                .await
                .expect("nil")
                .is_empty()
        );

        // The batch form groups alternates under their primary; primaries with
        // no alternates are absent.
        let batch = repository
            .get_items_by_primary_version_batch(&[primary, unrelated])
            .await
            .expect("batch");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[&primary].len(), 1);
        assert_eq!(batch[&primary][0].id, guid_to_db(alt));
        assert!(!batch.contains_key(&unrelated));
    }

    #[tokio::test]
    async fn by_name_total_survives_offset_past_end() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0xA0F1);
        seed_named_item(&db, movie, BaseItemKind::Movie, "One").await;
        for g in ["Action", "Adventure", "Comedy", "Drama", "Horror"] {
            seed_item_genre(&db, movie, g).await;
        }

        let past = InternalItemsQuery {
            start_index: Some(50),
            limit: Some(2),
            ..Default::default()
        };
        let r = repository.get_genres(&past).await.expect("past");
        assert!(r.items.is_empty());
        assert_eq!(
            r.total_record_count, 5,
            "total must survive an offset past the end"
        );

        let nomatch = InternalItemsQuery {
            search_term: Some("ZZZZ".to_owned()),
            limit: Some(2),
            ..Default::default()
        };
        let z = repository.get_genres(&nomatch).await.expect("z");
        assert_eq!(
            z.total_record_count, 0,
            "genuine empty is 0, not a stale count"
        );
    }
}
