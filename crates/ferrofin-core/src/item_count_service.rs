//! [`FerrofinItemCountService`] — the concrete [`ItemCountService`].
//!
//! Port of `ItemCountService`. Counts items matching an [`InternalItemsQuery`]
//! and rolls the per-`Type` counts up into an [`ItemCounts`] the same way C#
//! `GetItemCounts` does. The linked-children count follows a box set's or
//! playlist's links through the `DescendantOfId` closure, as v12.2 does. The
//! per-page played/total batch (`get_played_and_total_count_batch`) unions
//! v12.2's three branches: the `AncestorIds` closure, the `LinkedChildren`, and
//! the closure of each linked folder, each page folder standing for its merged
//! presentation-key group. See the TODO on `folder_leaf_count_sql` for where
//! it still differs from v12.2. A folder the
//! batch leaves out (0/0) is counted per item, as `Folder.FillUserDataDtoValues`
//! does, through `get_played_and_total_count` or its linked sibling.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::ItemValueType;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::ItemCounts;
use sqlx::{Execute, QueryBuilder, Sqlite};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ExtraCounts, ItemCountService, NameItemRow, PlayedAndTotal};

use crate::aggregate_folder::RootFolderIds;
use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::{
    ALTERNATE_VERSION_HIDDEN, QueryShape, TopParentTerm, append_parental_restrictions, build_query,
    push_access_filter,
};

/// Re-opens a bound aggregate and lets `push` add predicates before its
/// GROUP BY. The suffix contains no parameters, so the original bind order is
/// retained.
fn before_group_by<'q>(
    mut query: impl Execute<'q, Sqlite>,
    push: impl FnOnce(&mut QueryBuilder<Sqlite>),
) -> Result<QueryBuilder<Sqlite>, ServiceError> {
    let arguments = query
        .take_arguments()
        .map_err(|error| ServiceError::backend(error.to_string()))?
        .unwrap_or_default();
    let sql = query.sql();
    let (prefix, suffix) = sql
        .as_str()
        .rsplit_once(" GROUP BY ")
        .map_or((sql.as_str(), None), |(prefix, suffix)| {
            (prefix, Some(suffix))
        });
    let mut qb = QueryBuilder::with_arguments(prefix, arguments);
    push(&mut qb);
    if let Some(suffix) = suffix {
        qb.push(" GROUP BY ").push(suffix);
    }
    Ok(qb)
}

/// Adds the same rating predicate as browsing before an existing aggregate's
/// GROUP BY ([`before_group_by`]). Related-id aggregates correlate a
/// primary-key lookup; the child-count aggregates and the whole-server played
/// arm already read the counted row as `bi`.
fn restrict_count_query<'q>(
    query: impl Execute<'q, Sqlite>,
    filter: &InternalItemsQuery,
    related_id: Option<&str>,
) -> Result<QueryBuilder<Sqlite>, ServiceError> {
    before_group_by(query, |qb| {
        if !crate::translate_query::has_parental_restrictions(filter) {
            return;
        }
        if let Some(id) = related_id {
            qb.push(format!(
                r#" AND EXISTS (SELECT 1 FROM "BaseItems" bi WHERE bi."Id" = {id}"#
            ));
        }
        append_parental_restrictions(qb, filter, "bi");
        if related_id.is_some() {
            qb.push(")");
        }
    })
}

/// A by-name count aggregate over v12.2 `ApplyAccessFiltering`, the
/// related items `ItemCountService.GetItemCountsForNameItems` counts
/// (`ItemCountService.cs:172-176`), added before its GROUP BY: the libraries
/// [`crate::item_repository::scope_to_user_libraries`] put in
/// `filter.top_parent_ids` for its user (`LibraryManager.AddUserToQuery`;
/// none with no user), the parental rules, and no alternate version or owned
/// non-extra, whoever asks ([`push_access_filter`]).
///
/// The library scope is a residual check: each statement already reaches
/// `BaseItems` from the names asked for, by primary key or by `Type`, and
/// keeps that plan (`access_filtered_name_counts_keep_their_seeks`).
fn access_filtered_count_query<'q>(
    query: impl Execute<'q, Sqlite>,
    filter: &InternalItemsQuery,
) -> Result<QueryBuilder<Sqlite>, ServiceError> {
    before_group_by(query, |qb| {
        push_access_filter(qb, filter, &filter.top_parent_ids, TopParentTerm::Residual);
    })
}

/// `BaseItem.DisplayExtraTypes`: Unknown, Clip, BehindTheScenes,
/// DeletedScene, Interview, Scene, Sample, Featurette and Short. Trailer (2)
/// has its own count; theme media (8/9) and NULL are excluded. The OwnerId
/// index keeps the query scoped to the requested page, including parentless extras.
fn extra_counts_sql(owner_count: usize) -> String {
    format!(
        r#"SELECT "OwnerId",
            SUM(CASE WHEN "ExtraType" IN (0,1,3,4,5,6,7,10,11) THEN 1 ELSE 0 END),
            SUM(CASE WHEN "ExtraType" = 2 THEN 1 ELSE 0 END)
           FROM "BaseItems" WHERE "OwnerId" IN ({}) GROUP BY "OwnerId""#,
        placeholders(owner_count)
    )
}

/// v12.2 `GetChildCountBatch`'s `accessibleItems` on the `bi` row, short of
/// the parental and tag rules (which [`restrict_count_query`] adds).
///
/// With no user it is `DescendantQueryHelper.IsDistinctLibraryItem`: no
/// alternate version, and no owned part that is not an extra. With a user it
/// is `ApplyAccessFiltering`'s pair, which the browse applies too
/// ([`ALTERNATE_VERSION_HIDDEN`]): an alternate is hidden only while its
/// primary is in the same library, and an owned non-extra is hidden.
///
/// `ApplyAccessFiltering` also starts with `ApplyTopParentFiltering`, which
/// keeps only rows in libraries the user may open. That part is not ported
/// yet (GitLab gl-799): it needs the user's visible libraries resolved once
/// per batch, as the browse does.
fn accessible_item(has_user: bool) -> String {
    let owned = r#"(bi."OwnerId" IS NULL OR bi."ExtraType" IS NOT NULL)"#;
    if has_user {
        format!("{owned}{ALTERNATE_VERSION_HIDDEN}")
    } else {
        format!(r#"bi."PrimaryVersionId" IS NULL AND {owned}"#)
    }
}

/// Which column ties a child to the parent it is counted under in
/// `GetChildCountBatch`.
#[derive(Clone, Copy)]
enum ChildOf {
    /// Its `ParentId`, for every row not tied to a season.
    Parent,
    /// Its `SeasonId`: an episode is a child of its season even when it is
    /// filed under the series, as a flat series folder files it.
    Season,
}

impl ChildOf {
    /// The column a child row is grouped on, and the term that keeps the two
    /// arms apart. The `+` pins the grouped column's own index: `"SeasonId"
    /// IS NULL` holds for most rows, so an index on it would walk nearly the
    /// whole table, and no later index or statistics may choose one.
    fn column_and_term(self) -> (&'static str, &'static str) {
        match self {
            Self::Parent => ("ParentId", r#" AND +bi."SeasonId" IS NULL"#),
            Self::Season => ("SeasonId", ""),
        }
    }
}

/// The `IsVirtualItem` term of `GetChildCountBatch`'s `includeVirtual`: a
/// missing episode counts only for no user or one who displays them.
fn virtual_term(include_virtual: bool) -> &'static str {
    if include_virtual {
        ""
    } else {
        r#" AND +bi."IsVirtualItem" = 0"#
    }
}

/// v12.2 `GetChildCountBatch`'s `hierarchicalCounts` and `seasonCounts`: the
/// [`accessible_item`] children of `n` parents, grouped on `child_of`'s
/// column. Merged alternates do not count (a season's `ChildCount` is its
/// distinct episodes, not every version), nor does a stacked part or local
/// version, which belongs to its owner.
fn child_count_sql(child_of: ChildOf, n: usize, has_user: bool, include_virtual: bool) -> String {
    let (column, term) = child_of.column_and_term();
    format!(
        r#"SELECT bi."{column}", COUNT(*) FROM "BaseItems" bi
           WHERE {access}{term}{virtual} AND bi."{column}" IN ({})
           GROUP BY bi."{column}""#,
        placeholders(n),
        access = accessible_item(has_user),
        virtual = virtual_term(include_virtual),
    )
}

/// `GetChildCountBatch`'s `linkedCounts`: the `LinkedChildren` rows of `n`
/// parents whose child is an [`accessible_item`] ("a linked child counts
/// only when the item it points at is one the user may open").
fn linked_count_sql(n: usize, has_user: bool) -> String {
    format!(
        // `CROSS JOIN` keeps the page's links as the outer loop, so each
        // child is one primary-key probe.
        r#"SELECT lc."ParentId", COUNT(*) FROM "LinkedChildren" lc
           CROSS JOIN "BaseItems" bi ON bi."Id" = lc."ChildId"
           WHERE {access} AND lc."ParentId" IN ({})
           GROUP BY lc."ParentId""#,
        placeholders(n),
        access = accessible_item(has_user),
    )
}

/// `GetMergedChildCounts`' child rows for `n` merged folders: the parent each
/// is counted under, its id and its `PresentationUniqueKey`.
fn merged_children_sql(
    child_of: ChildOf,
    n: usize,
    has_user: bool,
    include_virtual: bool,
) -> String {
    let (column, term) = child_of.column_and_term();
    format!(
        r#"SELECT bi."{column}", bi."Id", bi."PresentationUniqueKey" FROM "BaseItems" bi
           WHERE {access}{term}{virtual} AND bi."{column}" IN ({})"#,
        placeholders(n),
        access = accessible_item(has_user),
        virtual = virtual_term(include_virtual),
    )
}

/// The concrete item-count service.
#[derive(Clone)]
pub struct FerrofinItemCountService {
    db: Database,
    /// The `UserRootFolder`/`AggregateFolder` ids, injected once by the
    /// composition root (see [`FerrofinItemCountService::with_root_ids`]).
    roots: Option<RootFolderIds>,
    /// The `IsVisible` evaluator the count's library scope asks about
    /// playlists and box sets (see [`FerrofinItemCountService::with_visibility`]).
    visibility: crate::item_repository::VisibilitySlot,
}

impl std::fmt::Debug for FerrofinItemCountService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinItemCountService")
            .finish_non_exhaustive()
    }
}

impl FerrofinItemCountService {
    /// Creates the service over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self {
            db,
            roots: None,
            visibility: crate::item_repository::VisibilitySlot::default(),
        }
    }

    /// Installs the slot the composition root fills with the server's
    /// `ItemVisibility`, the same one the item repository holds, so a count's
    /// library scope admits the playlists folder and a box-set library exactly
    /// when a query's does (`UserViewManager.GetUserViews`). Left empty, both
    /// are admitted.
    #[must_use]
    pub fn with_visibility(mut self, slot: crate::item_repository::VisibilitySlot) -> Self {
        self.visibility = slot;
        self
    }

    /// Injects the two root-folder ids resolved once at startup, so the user
    /// root's `ChildCount` includes the `AggregateFolder`'s virtual children.
    ///
    /// Both are process constants; the extra work is one id comparison per
    /// requested parent, and one additional id in the grouped count that is
    /// already being issued — never a third round-trip.
    #[must_use]
    pub fn with_root_ids(mut self, roots: RootFolderIds) -> Self {
        self.roots = Some(roots);
        self
    }

    /// `COUNT(*)` over the rows an already-resolved `filter` matches (its
    /// preferences and `DescendantOfId` roots loaded) — ungrouped, and with no
    /// library scope added (unlike [`ItemCountService::get_count`]).
    async fn count_rows(&self, filter: &InternalItemsQuery) -> Result<i32, ServiceError> {
        let mut qb = build_query(filter, QueryShape::Count);
        let count: i64 = qb
            .build_query_scalar::<i64>()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
    }

    /// The countable leaves (`IsCountableLeaf`: neither a folder nor virtual)
    /// below `parent_id`'s closure and links, with `filter`'s preferences and
    /// the `DescendantOfId` roots resolved — the statement
    /// [`ItemCountService::get_played_and_total_count_from_linked_children`]
    /// counts twice (total, then played).
    async fn countable_leaves_query(
        &self,
        filter: &InternalItemsQuery,
        parent_id: Uuid,
    ) -> Result<InternalItemsQuery, ServiceError> {
        let leaves = InternalItemsQuery {
            descendant_of_id: Some(parent_id),
            is_folder: Some(false),
            is_virtual_item: Some(false),
            ..filter.clone()
        };
        let leaves = match crate::query_restrictions::resolve(&self.db, &leaves).await? {
            Some(resolved) => resolved,
            None => leaves,
        };
        Ok(
            match crate::item_repository::resolve_descendant_query(&self.db, &leaves).await? {
                Some(resolved) => resolved,
                None => leaves,
            },
        )
    }

    /// The folders sharing `folder_id`'s `PresentationUniqueKey`
    /// ([`Self::presentation_key_groups`] for one folder).
    async fn presentation_key_group(&self, folder_id: Uuid) -> Result<Vec<Uuid>, ServiceError> {
        Ok(self
            .presentation_key_groups(&[folder_id])
            .await?
            .remove(&folder_id)
            .unwrap_or_else(|| vec![folder_id]))
    }

    /// Each of `folder_ids` with the folders sharing its
    /// `PresentationUniqueKey`: a merged series or season is one row per
    /// library (v12.2 `GetPresentationKeyGroups`, `ItemCountService.cs:
    /// 693-733`). A folder with no key, or whose key matches no folder, is a
    /// group of one. One statement for all of them
    /// ([`presentation_key_groups_sql`]).
    async fn presentation_key_groups(
        &self,
        folder_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
        let ids: Vec<String> = folder_ids.iter().copied().map(guid_to_db).collect();
        let page = serde_json::to_string(&ids).map_err(ServiceError::backend_source)?;
        let rows: Vec<(String, String)> = sqlx::query_as(presentation_key_groups_sql())
            .bind(page)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        let parse =
            |id: &str| Uuid::parse_str(id).map_err(|e| ServiceError::backend(e.to_string()));
        let mut groups: HashMap<Uuid, Vec<Uuid>> = HashMap::with_capacity(folder_ids.len());
        for (folder, member) in rows {
            let (folder, member) = (parse(&folder)?, parse(&member)?);
            let members = groups.entry(folder).or_default();
            // A page that lists a folder twice reads its group twice.
            if !members.contains(&member) {
                members.push(member);
            }
        }
        for &folder in folder_ids {
            groups.entry(folder).or_insert_with(|| vec![folder]);
        }
        Ok(groups)
    }

    /// Counts each person's credited items by type via `PeopleBaseItemMap`→`Peoples`,
    /// keyed on the person row's `Name` (C# `ItemCountService` Person branch —
    /// `m.People.Name == item.Name`). People are not in `ItemValues`, so the generic
    /// CleanName/ItemValues count path returns zero for them.
    async fn people_name_counts(
        &self,
        rows: &[NameItemRow<'_>],
        related_item_kinds: &[BaseItemKind],
        mut out: HashMap<Uuid, ItemCounts>,
        access_filter: &InternalItemsQuery,
    ) -> Result<HashMap<Uuid, ItemCounts>, ServiceError> {
        // Each row's Name comes from the caller's already-projected row — the
        // page was read out of `BaseItems` to build the DTOs, so re-selecting
        // `Id`/`Name` here would be a round trip for data in hand.
        let name_by_id: Vec<(Uuid, &str)> = rows
            .iter()
            .filter_map(|row| row.name.map(|name| (row.id, name)))
            .collect();
        if name_by_id.is_empty() {
            return Ok(out);
        }

        let type_names: Vec<&'static str> = related_item_kinds
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .collect();
        // Dedupe by borrowing from `name_by_id` — the names are bound straight
        // through to sqlx as `&str`, so no name is ever copied on this path.
        let distinct_names: Vec<&str> = name_by_id
            .iter()
            .map(|(_, n)| *n)
            .collect::<HashSet<&str>>()
            .into_iter()
            .collect();

        let mut by_name: HashMap<String, HashMap<String, i32>> = HashMap::new();
        for chunk in distinct_names.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = people_name_counts_sql(chunk.len(), type_names.len());

            let mut query = sqlx::query_as::<_, (String, String, i64)>(sqlx::AssertSqlSafe(sql));
            for name in chunk {
                query = query.bind(*name);
            }
            for t in &type_names {
                query = query.bind(*t);
            }
            let mut restricted = access_filtered_count_query(query, access_filter)?;
            for (name, type_, count) in restricted
                .build_query_as::<(String, String, i64)>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
            {
                by_name
                    .entry(name)
                    .or_default()
                    .insert(type_, i32::try_from(count).unwrap_or(i32::MAX));
            }
        }

        for (id, name) in name_by_id {
            if let Some(by_type) = by_name.get(name) {
                out.insert(id, counts_from_type_map(by_type));
            }
        }
        Ok(out)
    }

    /// Counts each `Year` item's items by type off `BaseItems."ProductionYear"`
    /// — the C# `SetItemByNameInfo` Year arm (`query.Years = [year]`).
    ///
    /// A row whose `Name` does not parse as an integer keeps the all-zero
    /// default, matching the `when int.TryParse(...)` guard that makes the C#
    /// switch fall through to `default: return;` and leave the counts unset.
    async fn production_year_counts(
        &self,
        rows: &[NameItemRow<'_>],
        related_item_kinds: &[BaseItemKind],
        mut out: HashMap<Uuid, ItemCounts>,
        access_filter: &InternalItemsQuery,
    ) -> Result<HashMap<Uuid, ItemCounts>, ServiceError> {
        // The row's Name is already in hand from the page the caller projected.
        let year_by_id: Vec<(Uuid, i64)> = rows
            .iter()
            .filter_map(|row| {
                row.name
                    .and_then(|name| name.trim().parse::<i32>().ok())
                    .map(|year| (row.id, i64::from(year)))
            })
            .collect();
        if year_by_id.is_empty() {
            return Ok(out);
        }

        let type_names: Vec<&'static str> = related_item_kinds
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .collect();

        let distinct_years: Vec<i64> = year_by_id
            .iter()
            .map(|(_, y)| *y)
            .collect::<HashSet<i64>>()
            .into_iter()
            .collect();

        let mut by_year: HashMap<i64, HashMap<String, i32>> = HashMap::new();
        for chunk in distinct_years.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = production_year_counts_sql(chunk.len(), type_names.len());
            let mut query = sqlx::query_as::<_, (i64, String, i64)>(sqlx::AssertSqlSafe(sql));
            for year in chunk {
                query = query.bind(*year);
            }
            for t in &type_names {
                query = query.bind(*t);
            }
            let mut restricted = access_filtered_count_query(query, access_filter)?;
            for (year, type_, count) in restricted
                .build_query_as::<(i64, String, i64)>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
            {
                by_year
                    .entry(year)
                    .or_default()
                    .insert(type_, i32::try_from(count).unwrap_or(i32::MAX));
            }
        }

        for (id, year) in year_by_id {
            if let Some(by_type) = by_year.get(&year) {
                out.insert(id, counts_from_type_map(by_type));
            }
        }
        Ok(out)
    }

    /// v12.2 `CountEpisodesOfTaggedSeriesByCleanName`
    /// (`ItemCountService.cs:351-407`): per clean value, the distinct episodes under a series that
    /// carries it, and those of them that carry it themselves. Series and
    /// episodes alike are the related items the counts read
    /// ([`tagged_series_episodes_query`]). A value with no such episode is
    /// absent.
    async fn episodes_of_tagged_series(
        &self,
        cleans: &[&str],
        value_types: &[ItemValueType],
        access_filter: &InternalItemsQuery,
    ) -> Result<HashMap<String, TaggedSeriesEpisodes>, ServiceError> {
        let mut out = HashMap::new();
        for chunk in cleans.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let mut qb = tagged_series_episodes_query(chunk, value_types, access_filter);
            for (clean, of_tagged, tagged_of_tagged) in qb
                .build_query_as::<(String, i64, i64)>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
            {
                out.insert(
                    clean,
                    TaggedSeriesEpisodes {
                        of_tagged_series: i32::try_from(of_tagged).unwrap_or(i32::MAX),
                        tagged_of_tagged_series: i32::try_from(tagged_of_tagged)
                            .unwrap_or(i32::MAX),
                    },
                );
            }
        }
        Ok(out)
    }
}

/// One clean value's episode roll-up: [`FerrofinItemCountService::episodes_of_tagged_series`].
#[derive(Clone, Copy, Default)]
struct TaggedSeriesEpisodes {
    /// The distinct episodes under a series carrying the value.
    of_tagged_series: i32,
    /// Those of them that carry the value themselves.
    tagged_of_tagged_series: i32,
}

/// v12.2 `RollsUpEpisodes` (`ItemCountService.cs:346-349`): a studio or genre
/// (or music genre) counted over episodes and series counts the episodes of
/// the series it is written on.
fn rolls_up_episodes(kind: BaseItemKind, related_item_kinds: &[BaseItemKind]) -> bool {
    matches!(
        kind,
        BaseItemKind::Studio | BaseItemKind::Genre | BaseItemKind::MusicGenre
    ) && related_item_kinds.contains(&BaseItemKind::Episode)
        && related_item_kinds.contains(&BaseItemKind::Series)
}

/// The episode roll-up for `cleans`: per clean value, the distinct episodes
/// whose `SeriesId` is a series carrying it (`episodesOfTaggedSeries`), and
/// those of them that carry it themselves (`taggedEpisodesOfTaggedSeries`).
///
/// Both the series and the episodes pass [`access_filtered_count_query`]'s
/// terms, as `related` does upstream. The tagged series (`ts`) seed from
/// `ItemValues`, as [`item_value_counts_sql`] does, reach `BaseItems` by
/// primary key, and are made distinct (an item may carry a value through two
/// value types); `CROSS JOIN` keeps them the outer loop, each probing its
/// episodes through `IX_BaseItems_SeriesId`. The `+` on the episode's `Type`
/// keeps it off the `Type`-led composites, which would walk every episode on
/// the server once per tagged series. `EXPLAIN QUERY PLAN` pinned by
/// `the_tagged_series_roll_up_seeks_from_the_values`.
fn tagged_series_episodes_query(
    cleans: &[&str],
    value_types: &[ItemValueType],
    access_filter: &InternalItemsQuery,
) -> QueryBuilder<Sqlite> {
    let value_types = value_types
        .iter()
        .map(|t| i32::from(*t).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let series = stored_type_name(BaseItemKind::Series).unwrap_or_default();
    let episode = stored_type_name(BaseItemKind::Episode).unwrap_or_default();
    // Bound discriminants, not user input, inlined as in `item_value_counts_sql`.
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
        r#"SELECT ts."CleanValue", COUNT(DISTINCT bi."Id"),
                  COUNT(DISTINCT CASE WHEN EXISTS (
                      SELECT 1 FROM "ItemValuesMap" em
                      CROSS JOIN "ItemValues" ev ON ev."ItemValueId" = em."ItemValueId"
                      WHERE em."ItemId" = bi."Id" AND ev."CleanValue" = ts."CleanValue"
                        AND ev."Type" IN ({value_types})) THEN bi."Id" END)
           FROM (SELECT DISTINCT iv."CleanValue" AS "CleanValue", bi."Id" AS "SeriesId"
                 FROM "ItemValues" iv
                 CROSS JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
                 CROSS JOIN "BaseItems" bi ON bi."Id" = ivm."ItemId"
                 WHERE iv."Type" IN ({value_types}) AND "#
    ));
    crate::translate_query::push_in_list(&mut qb, r#"iv."CleanValue""#, cleans);
    qb.push(r#" AND bi."Type" = "#).push_bind(series);
    push_access_filter(
        &mut qb,
        access_filter,
        &access_filter.top_parent_ids,
        TopParentTerm::Residual,
    );
    qb.push(
        r#") ts
           CROSS JOIN "BaseItems" bi ON bi."SeriesId" = ts."SeriesId"
           WHERE +bi."Type" = "#,
    )
    .push_bind(episode);
    push_access_filter(
        &mut qb,
        access_filter,
        &access_filter.top_parent_ids,
        TopParentTerm::Residual,
    );
    qb.push(r#" GROUP BY ts."CleanValue""#);
    qb
}

#[async_trait]
impl ItemCountService for FerrofinItemCountService {
    async fn get_extra_counts_batch(
        &self,
        owner_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ExtraCounts>, ServiceError> {
        let mut counts = HashMap::new();
        for chunk in owner_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = extra_counts_sql(chunk.len());
            let mut query = sqlx::query_as::<_, (String, i64, i64)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for (owner, special_features, local_trailers) in
                query.fetch_all(self.db.pool()).await.map_err(db_err)?
            {
                if let Ok(owner) = Uuid::parse_str(&owner) {
                    counts.insert(
                        owner,
                        ExtraCounts {
                            special_features: i32::try_from(special_features).unwrap_or(i32::MAX),
                            local_trailers: i32::try_from(local_trailers).unwrap_or(i32::MAX),
                        },
                    );
                }
            }
        }
        Ok(counts)
    }

    async fn get_count(&self, filter: &InternalItemsQuery) -> Result<i32, ServiceError> {
        // Same `AddUserToQuery` confinement the item queries get: a user who may
        // see one library must be counted over that library, not the server.
        // Without it `/Items/Counts` handed a restricted account the whole
        // catalogue's totals — numbers it could not then browse to.
        let visibility = crate::item_repository::bound_visibility(&self.visibility);
        let scoped = crate::item_repository::scope_to_user_libraries(
            &self.db,
            visibility.as_deref(),
            filter,
        )
        .await?;
        let filter = scoped.as_ref().unwrap_or(filter);
        let descendants =
            crate::item_repository::resolve_descendant_query(&self.db, filter).await?;
        let filter = descendants.as_ref().unwrap_or(filter);
        let mut qb = build_query(filter, QueryShape::Count);
        let count: i64 = qb
            .build_query_scalar::<i64>()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
    }

    async fn get_item_counts(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        // Group by stored Type in SQL — one aggregate query returning ~a dozen
        // (type, count) rows — instead of materializing every matching full row
        // (all ~60 columns) and counting them in Rust, which dominated this
        // endpoint's CPU on a large library.
        let visibility = crate::item_repository::bound_visibility(&self.visibility);
        let scoped = crate::item_repository::scope_to_user_libraries(
            &self.db,
            visibility.as_deref(),
            filter,
        )
        .await?;
        let filter = scoped.as_ref().unwrap_or(filter);
        let descendants =
            crate::item_repository::resolve_descendant_query(&self.db, filter).await?;
        let filter = descendants.as_ref().unwrap_or(filter);
        let mut qb = build_query(filter, QueryShape::TypeCounts);
        let rows = qb
            .build_query_as::<(String, i64)>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        let by_type: HashMap<String, i32> = rows
            .into_iter()
            .map(|(t, c)| (t, i32::try_from(c).unwrap_or(i32::MAX)))
            .collect();

        // ItemCountService.GetItemCounts (v12.2, :73) sets ItemCount to the sum
        // of every type group, types with no field of their own included.
        Ok(counts_from_type_map(&by_type))
    }

    async fn get_item_counts_for_name_item(
        &self,
        kind: BaseItemKind,
        id: Uuid,
        related_item_kinds: &[BaseItemKind],
        access_filter: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        // A single by-name item is a batch of one; the caller passed only an
        // id, so this form is the one place that still reads the name columns.
        let names: Option<(Option<String>, Option<String>)> =
            sqlx::query_as::<_, (Option<String>, Option<String>)>(
                r#"SELECT "Name", "CleanName" FROM "BaseItems" WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .fetch_optional(self.db.pool())
            .await
            .map_err(db_err)?;
        let Some((name, clean_name)) = names else {
            return Ok(ItemCounts::default());
        };
        let row = NameItemRow {
            id,
            name: name.as_deref(),
            clean_name: clean_name.as_deref(),
        };
        Ok(self
            .get_item_counts_for_name_items(
                kind,
                std::slice::from_ref(&row),
                related_item_kinds,
                access_filter,
            )
            .await?
            .remove(&id)
            .unwrap_or_default())
    }

    async fn get_item_counts_for_name_items(
        &self,
        kind: BaseItemKind,
        rows: &[NameItemRow<'_>],
        related_item_kinds: &[BaseItemKind],
        access_filter: &InternalItemsQuery,
    ) -> Result<HashMap<Uuid, ItemCounts>, ServiceError> {
        // Every row reports counts (zeros when its CleanName is missing),
        // matching the per-item form's defaults.
        let mut out: HashMap<Uuid, ItemCounts> = rows
            .iter()
            .map(|row| (row.id, ItemCounts::default()))
            .collect();
        if rows.is_empty() {
            return Ok(out);
        }
        // v12.2 `LibraryManager.GetItemCountsForNameItems` runs
        // `AddUserToQuery` for a user, so the counts cover the libraries that
        // user may open; with no user (an API key) they cover the server.
        // The preferences come resolved with it.
        let visibility = crate::item_repository::bound_visibility(&self.visibility);
        let scoped = crate::item_repository::scope_to_user_libraries(
            &self.db,
            visibility.as_deref(),
            access_filter,
        )
        .await?;
        let access_filter = scoped.as_ref().unwrap_or(access_filter);

        // People live in `PeopleBaseItemMap`/`Peoples`, not `ItemValues`, so a Person's
        // filmography is counted by joining the people map on the person's Name — the
        // C# `ItemCountService` Person branch (`m.People.Name == item.Name`). The
        // CleanName/ItemValues path below would count zero for a Person.
        if kind == BaseItemKind::Person {
            return self
                .people_name_counts(rows, related_item_kinds, out, access_filter)
                .await;
        }
        // A `Year` counts by `ProductionYear`, not by a value name
        // (`ItemCountService.cs:170-181`); routing it through the clean-value
        // aggregate below answered 0 for every year that has content.
        //
        // A Year is not an `ItemValues` row either. The pinned 10.11.8
        // `ItemValueType` enum is Artist=0, AlbumArtist=1, Genre=2, Studios=3,
        // Tags=4, InheritedTags=6 — no Year — so the CleanName join further down
        // matched nothing and every `/Years/{year}` reported `ChildCount: 0`.
        // C# `DtoService.SetItemByNameInfo` counts a Year by the production-year
        // column instead: `case BaseItemKind.Year when int.TryParse(dto.Name,
        // NumberStyles.Integer, CultureInfo.InvariantCulture, out var year):
        // query.Years = [year];`.
        if kind == BaseItemKind::Year {
            return self
                .production_year_counts(rows, related_item_kinds, out, access_filter)
                .await;
        }
        // The C# `switch`'s `default:` arm returns an empty `ItemCounts`. Any
        // other kind reaching here would otherwise be counted by a name match
        // against every value type at once.
        let Some(value_types) = by_name_count_value_types(kind) else {
            return Ok(out);
        };

        // Each row's CleanName rides along on the row the caller is already
        // projecting, so this path reads `ItemValues` and nothing else.
        let clean_by_id: Vec<(Uuid, &str)> = rows
            .iter()
            .filter_map(|row| row.clean_name.map(|clean| (row.id, clean)))
            .collect();
        if clean_by_id.is_empty() {
            return Ok(out);
        }

        let type_names: Vec<&'static str> = related_item_kinds
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .collect();

        // Count the related items carrying each clean value, grouped by value
        // and type — one query per chunk covers the whole page.
        // Apply access predicates inside SQL before aggregating each value.
        // Dedupe by borrowing from `clean_by_id`; the chunks bind straight through
        // to sqlx as `&str`, so no clean value is copied on this path.
        let distinct_cleans: Vec<&str> = clean_by_id
            .iter()
            .map(|(_, c)| *c)
            .collect::<HashSet<&str>>()
            .into_iter()
            .collect();
        let mut by_clean: HashMap<String, HashMap<String, i32>> = HashMap::new();
        for chunk in distinct_cleans.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = item_value_counts_sql(chunk.len(), type_names.len(), value_types);

            let mut query = sqlx::query_as::<_, (String, String, i64)>(sqlx::AssertSqlSafe(sql));
            for clean in chunk {
                query = query.bind(*clean);
            }
            for t in &type_names {
                query = query.bind(*t);
            }
            let mut restricted = access_filtered_count_query(query, access_filter)?;
            for (clean, type_, count) in restricted
                .build_query_as::<(String, String, i64)>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
            {
                by_clean
                    .entry(clean)
                    .or_default()
                    .insert(type_, i32::try_from(count).unwrap_or(i32::MAX));
            }
        }

        // v12.2 `CountByItemValue` (`ItemCountService.cs:227-252`): a studio
        // or genre is usually written on the series, so the episodes of a
        // series carrying the value count towards it, once the page has
        // counted a series at all.
        let series_type = stored_type_name(BaseItemKind::Series).unwrap_or_default();
        let roll_up = if rolls_up_episodes(kind, related_item_kinds)
            && by_clean
                .values()
                .any(|by_type| by_type.contains_key(series_type))
        {
            Some(
                self.episodes_of_tagged_series(&distinct_cleans, value_types, access_filter)
                    .await?,
            )
        } else {
            None
        };

        for (id, clean) in clean_by_id {
            if let Some(by_type) = by_clean.get(clean) {
                let mut counts = counts_from_type_map(by_type);
                if let Some(roll_up) = &roll_up {
                    let tagged = roll_up.get(clean).copied().unwrap_or_default();
                    // A tagged episode of a tagged series is counted by the
                    // roll-up already, so it comes off the direct ones.
                    let episodes = tagged
                        .of_tagged_series
                        .saturating_add(counts.episode_count)
                        .saturating_sub(tagged.tagged_of_tagged_series);
                    // `ItemCountBuilder.SetEpisodeCount` moves the total with it.
                    counts.item_count = counts
                        .item_count
                        .saturating_add(episodes)
                        .saturating_sub(counts.episode_count);
                    counts.episode_count = episodes;
                }
                out.insert(id, counts);
            }
        }
        Ok(out)
    }

    async fn get_played_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<i32, ServiceError> {
        Ok(self
            .get_played_and_total_count(filter, ancestor_id)
            .await?
            .played)
    }

    async fn get_total_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<i32, ServiceError> {
        Ok(self
            .get_played_and_total_count(filter, ancestor_id)
            .await?
            .total)
    }

    /// Port of v12.2 `GetPlayedAndTotalCount` over
    /// `BuildGroupedDescendantsQuery` (`ItemCountService.cs:430-452`): the
    /// countable leaves below every folder in `ancestor_id`'s presentation-key
    /// group, each walked as
    /// [`ItemCountService::get_played_and_total_count_from_linked_children`]
    /// walks its parent (`GetAllDescendantIdsBatch`, closure and links).
    async fn get_played_and_total_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<PlayedAndTotal, ServiceError> {
        // The `AggregateFolder`'s leaves are counted by their roots, as in
        // `get_played_and_total_count_batch` (`whole_server_leaf_counts`).
        if let (Some(roots), Some(user)) = (self.roots, filter.user.as_ref())
            && roots.aggregate == ancestor_id
        {
            return self.whole_server_leaf_counts(user, roots).await;
        }
        // ponytail: summed per member where upstream unions the members' leaves
        // first; members are distinct merged folders, whose leaves cannot
        // overlap (the batch sums them the same way, `ItemCountService.cs:669`).
        // Resolved once for every member.
        let resolved = crate::query_restrictions::resolve(&self.db, filter).await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        let mut sum = PlayedAndTotal::default();
        for member in self.presentation_key_group(ancestor_id).await? {
            let counts = self
                .get_played_and_total_count_from_linked_children(filter, member)
                .await?;
            sum.played = sum.played.saturating_add(counts.played);
            sum.total = sum.total.saturating_add(counts.total);
        }
        Ok(sum)
    }

    /// Port of v12.2 `GetPlayedAndTotalCountFromLinkedChildren`
    /// (`ItemCountService.cs:454-467`): every item below `parent_id`'s links and
    /// below what they link (`GetAllDescendantIdsBatch`, the `DescendantOfId`
    /// closure), kept to the countable leaves (`IsCountableLeaf`: neither a
    /// folder nor virtual) and access-filtered for the filter's user. Rows, not
    /// titles, and no library scope: the caller's `InternalItemsQuery(user)`
    /// names no top parents. Played is the leaf `IsPlayed` predicate (the port
    /// of `BuildLeafIsPlayedFilter`), where upstream's per-item path uses
    /// `IsPlayedBy`: the two agree on primary versions, but an alternate
    /// version listed on its own (its primary in another library, or gone)
    /// also counts as played here through its primary or a sibling.
    async fn get_played_and_total_count_from_linked_children(
        &self,
        filter: &InternalItemsQuery,
        parent_id: Uuid,
    ) -> Result<PlayedAndTotal, ServiceError> {
        // Resolved once for both counts.
        let leaves = self.countable_leaves_query(filter, parent_id).await?;
        let total = self.count_rows(&leaves).await?;
        if leaves.user.is_none() || total == 0 {
            return Ok(PlayedAndTotal { played: 0, total });
        }
        let played = self
            .count_rows(&InternalItemsQuery {
                is_played: Some(true),
                ..leaves
            })
            .await?;
        Ok(PlayedAndTotal { played, total })
    }

    async fn get_played_and_total_count_batch(
        &self,
        folder_ids: &[Uuid],
        user: &UserEntity,
    ) -> Result<HashMap<Uuid, PlayedAndTotal>, ServiceError> {
        let mut out = HashMap::with_capacity(folder_ids.len());
        if folder_ids.is_empty() {
            return Ok(out);
        }
        // Jellyfin's `Folder.GetUnplayedCount`/`GetPlayedPercentage` count only leaf
        // (media) descendants — a series' unplayed count is its unplayed episodes,
        // not its seasons — so we constrain to non-folder items.
        //
        // Two grouped joins over the `AncestorIds` and `LinkedChildren` branches
        // resolve the whole page in one pass each: total leaf descendants per
        // folder, then the played subset.
        // The prior path looped a per-folder descendant count, and each call
        // re-scanned every leaf in the library (a folder-independent id set) before
        // intersecting via a thousand-parameter `IN` — O(folders × library) with no
        // batching, which dominated the by-name browse endpoints.
        //
        // Each page folder stands for its merged presentation-key group (v12.2
        // `GetPresentationKeyGroups`): the branches count every member, and
        // each folder sums its members below.
        let groups = self.presentation_key_groups(folder_ids).await?;
        let mut members: Vec<Uuid> = groups.values().flatten().copied().collect();
        members.sort_unstable();
        members.dedup();
        let ids: Vec<String> = members.iter().copied().map(guid_to_db).collect();
        let page = serde_json::to_string(&ids).map_err(ServiceError::backend_source)?;

        let total_sql = folder_leaf_count_sql("", "");
        let total_q =
            sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(total_sql)).bind(page.as_str());
        let mut access_filter = InternalItemsQuery::default();
        access_filter.set_user(user.clone());
        let preferences = crate::query_restrictions::resolve(&self.db, &access_filter).await?;
        let access_filter = preferences.unwrap_or(access_filter);
        let mut total_q = restrict_count_query(total_q, &access_filter, Some(r#"a."ItemId""#))?;
        let totals = total_q
            .build_query_as::<(String, i64)>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        // Played subset: the same branches, joined to this user's played `UserData`.
        // `CROSS JOIN` is the join-order pin, not a different join — see
        // `folder_leaf_count_sql` for why the plain `JOIN` form drove from
        // `UserData` instead of from this page's parents. It cost 9.9 ms on a
        // 16-album `/Items/Latest` page and 55 ms on a 100-album one, against
        // 0.1 and 1.0 pinned.
        let played_sql = folder_leaf_count_sql(
            r#" CROSS JOIN "UserData" ud ON ud."ItemId" = a."ItemId""#,
            r#" AND ud."UserId" = ? AND ud."Played" = 1"#,
        );
        // The page's ids open the statement (its `WITH`), ahead of the `?` for UserId.
        let played_q = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(played_sql))
            .bind(page.as_str())
            .bind(user.id.as_str());
        let mut played_q = restrict_count_query(played_q, &access_filter, Some(r#"a."ItemId""#))?;
        let played_rows = played_q
            .build_query_as::<(String, i64)>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        let played_by_parent: HashMap<String, i64> = played_rows.into_iter().collect();

        let mut by_parent: HashMap<String, PlayedAndTotal> = HashMap::with_capacity(totals.len());
        for (parent, total) in totals {
            let played = played_by_parent.get(&parent).copied().unwrap_or(0);
            by_parent.insert(
                parent,
                PlayedAndTotal {
                    played: i32::try_from(played).unwrap_or(i32::MAX),
                    total: i32::try_from(total).unwrap_or(i32::MAX),
                },
            );
        }
        // Folders with no countable leaf in any branch are absent from the
        // grouped rows ⇒ 0/0: e.g. an empty folder, a season whose episodes are
        // all virtual (missing or unaired), a folder whose leaves the access
        // filter hides from this user, or a box set holding only box sets. v12.2
        // leaves them OUT of the batch (`if (total > 0 || played > 0)`,
        // `ItemCountService.cs:684`), and `Folder.FillUserDataDtoValues`
        // (Folder.cs:2042-2060) counts them per item through
        // `GetAllDescendantIdsBatch`, which follows a box set nested in a box
        // set where the branches here stop at the inner set's own closure.
        //
        // A group's members are distinct folders, so their leaves never
        // overlap and the sum is exact (`ItemCountService.cs:669-688`).
        for &folder in folder_ids {
            let mut sum = PlayedAndTotal {
                played: 0,
                total: 0,
            };
            for member in groups.get(&folder).map_or(&[][..], Vec::as_slice) {
                if let Some(counts) = by_parent.get(&guid_to_db(*member)) {
                    sum.played = sum.played.saturating_add(counts.played);
                    sum.total = sum.total.saturating_add(counts.total);
                }
            }
            if sum.total > 0 || sum.played > 0 {
                out.insert(folder, sum);
            }
        }
        // The `AggregateFolder` is the PHYSICAL root
        // (`AggregateFolder.IsPhysicalRoot`), the ancestor of every library row
        // but an extra. Its leaves are counted by their roots
        // (`whole_server_leaf_counts`): one extra pair of counts, and only when
        // the aggregate is on the page.
        if let Some(roots) = self.roots
            && folder_ids.contains(&roots.aggregate)
        {
            out.insert(
                roots.aggregate,
                self.whole_server_leaf_counts(user, roots).await?,
            );
        }
        Ok(out)
    }

    #[allow(clippy::too_many_lines)]
    async fn get_child_count_batch(
        &self,
        parent_ids: &[Uuid],
        user_id: Option<Uuid>,
    ) -> Result<HashMap<Uuid, i32>, ServiceError> {
        if parent_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut access_filter = InternalItemsQuery::default();
        if let Some(id) = user_id
            && let Some(user) = crate::item_repository::query_user(&self.db, id).await?
        {
            access_filter.set_user(user);
        }
        let has_user = access_filter.user.is_some();
        // `includeVirtual`: missing episodes count for no user, or for one
        // who displays them.
        let include_virtual = access_filter
            .user
            .as_ref()
            .is_none_or(|user| user.display_missing_episodes);
        let preferences = crate::query_restrictions::resolve(&self.db, &access_filter).await?;
        let access_filter = preferences.unwrap_or(access_filter);
        // C# `ItemCountService.GetChildCountBatch`: grouped counts of direct
        // `BaseItems` children, of the episodes tied to a season by `SeasonId`
        // (counted under the season, not the folder they are filed in), and of
        // `LinkedChildren` rows; a parent with linked children reports those
        // instead of its hierarchical children, and a merged folder the
        // distinct children of its whole group.
        //
        // A Jellyfin library is virtual, so its children hang off its physical
        // folders and grouping on the raw `ParentId` would report 0 for every
        // library on an adopted database — the same translation the item
        // repository does for a browse. The library itself is counted too:
        // a Ferrofin scan saves the rows it plans under the collection
        // folder, so an adopted library's children are split between the two
        // until every row is saved (`library_top_parents_by_view`; no row
        // hangs off both, so the sum counts each once). Empty, and so a
        // no-op, on a Ferrofin-written database.
        //
        // Two more translations come from the root pair. `Folder.GetChildren`
        // (Folder.cs:1348-1360) delegates the physical root's children to the
        // user root ("the true root should return our users root folder
        // children"), so the `AggregateFolder` counts as the `UserRootFolder`;
        // and `UserRootFolder.GetEligibleChildrenForRecursiveChildren`
        // (UserRootFolder.cs:96-102) concatenates the aggregate's virtual
        // children, so the user root's own count gains them.
        let as_user_root = |p: Uuid| match self.roots {
            Some(roots) if p == roots.aggregate => roots.user_root,
            _ => p,
        };
        let resolved_parents: Vec<Uuid> = parent_ids.iter().copied().map(as_user_root).collect();
        let by_view =
            crate::item_repository::library_top_parents_by_view(&self.db, &resolved_parents)
                .await?;
        let counted: Vec<Uuid> = resolved_parents
            .iter()
            .flat_map(|p| match by_view.get(p) {
                Some(folders) => folders.clone(),
                None => vec![*p],
            })
            .collect();
        let ids: Vec<String> = counted.iter().copied().map(guid_to_db).collect();
        let mut hierarchical: HashMap<String, i64> = HashMap::new();
        let mut seasons: HashMap<String, i64> = HashMap::new();
        let mut linked: HashMap<String, i64> = HashMap::new();
        // Chunked: an unpaged page can hold more parents than SQLite's bind
        // limit (32,766). `restrict_count_query` adds only the parental binds,
        // about four per tag in the user's tag policy plus a few tens, so a
        // chunk overflows only past ~8,000 tags on one user.
        for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let n = chunk.len();
            for (sql, into) in [
                (
                    child_count_sql(ChildOf::Parent, n, has_user, include_virtual),
                    &mut hierarchical,
                ),
                (
                    child_count_sql(ChildOf::Season, n, has_user, include_virtual),
                    &mut seasons,
                ),
                (linked_count_sql(n, has_user), &mut linked),
            ] {
                let mut query = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql));
                for id in chunk {
                    query = query.bind(id.as_str());
                }
                // Every statement reads the child as `bi`, so the parental and
                // tag rules apply to it directly.
                let mut query = restrict_count_query(query, &access_filter, None)?;
                into.extend(
                    query
                        .build_query_as::<(String, i64)>()
                        .fetch_all(self.db.pool())
                        .await
                        .map_err(db_err)?,
                );
            }
        }

        // The virtual children — one tiny indexed count, issued only when the
        // page actually carries a root row.
        let virtual_children = match self.roots {
            Some(roots)
                if resolved_parents.contains(&roots.user_root)
                    && roots.aggregate != roots.user_root =>
            {
                self.virtual_child_count(roots.aggregate).await?
            }
            _ => 0,
        };
        let merged = self
            .merged_child_counts(&resolved_parents, &access_filter, include_virtual)
            .await?;

        let mut out = HashMap::with_capacity(parent_ids.len());
        for (original, parent) in parent_ids.iter().zip(&resolved_parents) {
            if let Some(&count) = merged.get(parent) {
                out.insert(*original, i32::try_from(count).unwrap_or(i32::MAX));
                continue;
            }
            // A library sums its folders'; everything else counts its own row.
            let count: i64 = by_view
                .get(parent)
                .map_or_else(|| vec![*parent], Clone::clone)
                .iter()
                .map(|id| {
                    let key = guid_to_db(*id);
                    let linked_count = linked.get(&key).copied().unwrap_or(0);
                    if linked_count > 0 {
                        linked_count
                    } else {
                        hierarchical.get(&key).copied().unwrap_or(0)
                            + seasons.get(&key).copied().unwrap_or(0)
                    }
                })
                .sum();
            let count = count
                + if self.roots.is_some_and(|roots| *parent == roots.user_root) {
                    virtual_children
                } else {
                    0
                };
            out.insert(*original, i32::try_from(count).unwrap_or(i32::MAX));
        }
        Ok(out)
    }

    async fn get_linked_children_count_batch(
        &self,
        parent_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, i32>, ServiceError> {
        if parent_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // `Folder.LinkedChildren.Length` — the raw edge count, with no
        // hierarchical fallback and no library→physical-folder translation: a
        // library folder has no linked children, and the three kinds that read
        // this (MusicAlbum/Season/Playlist) are never virtual views.
        let ids: Vec<String> = parent_ids.iter().copied().map(guid_to_db).collect();
        let mut grouped: HashMap<String, i64> = HashMap::new();
        // Chunked: an unpaged page can hold more parents than SQLite's bind limit.
        for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let mut sql = String::from(
                r#"SELECT "ParentId", COUNT(*) FROM "LinkedChildren" WHERE "ParentId" IN ("#,
            );
            sql.push_str(&placeholders(chunk.len()));
            sql.push_str(r#") GROUP BY "ParentId""#);
            let mut query = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(id.as_str());
            }
            grouped.extend(query.fetch_all(self.db.pool()).await.map_err(db_err)?);
        }
        let mut out = HashMap::with_capacity(parent_ids.len());
        for parent in parent_ids {
            if let Some(count) = grouped.get(&guid_to_db(*parent)) {
                out.insert(*parent, i32::try_from(*count).unwrap_or(i32::MAX));
            }
        }
        Ok(out)
    }
}

impl FerrofinItemCountService {
    /// v12.2 `GetMergedChildCounts`: for each of `parents` whose
    /// `PresentationUniqueKey` is shared by other folders (a series or season
    /// that is one row per library), the distinct children of the whole
    /// group, a child's key being its `PresentationUniqueKey` or, without
    /// one, its id. Parents in a group of one are absent: they count their
    /// own rows.
    async fn merged_child_counts(
        &self,
        parents: &[Uuid],
        access_filter: &InternalItemsQuery,
        include_virtual: bool,
    ) -> Result<HashMap<Uuid, i64>, ServiceError> {
        let groups: Vec<(Uuid, Vec<Uuid>)> = self
            .presentation_key_groups(parents)
            .await?
            .into_iter()
            .filter(|(_, members)| members.len() > 1)
            .collect();
        if groups.is_empty() {
            return Ok(HashMap::new());
        }
        let mut members: Vec<String> = groups
            .iter()
            .flat_map(|(_, members)| members.iter().copied().map(guid_to_db))
            .collect();
        members.sort_unstable();
        members.dedup();
        let mut keys_by_member: HashMap<String, Vec<String>> = HashMap::new();
        for chunk in members.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            for child_of in [ChildOf::Parent, ChildOf::Season] {
                let sql = merged_children_sql(
                    child_of,
                    chunk.len(),
                    access_filter.user.is_some(),
                    include_virtual,
                );
                let mut query =
                    sqlx::query_as::<_, (String, String, Option<String>)>(sqlx::AssertSqlSafe(sql));
                for id in chunk {
                    query = query.bind(id.as_str());
                }
                let mut query = restrict_count_query(query, access_filter, None)?;
                let rows: Vec<(String, String, Option<String>)> = query
                    .build_query_as()
                    .fetch_all(self.db.pool())
                    .await
                    .map_err(db_err)?;
                for (parent, id, key) in rows {
                    let key = match key.filter(|key| !key.is_empty()) {
                        Some(key) => key,
                        // `Id.ToString("N")`.
                        None => Uuid::parse_str(&id).map_or(id, |id| id.simple().to_string()),
                    };
                    keys_by_member.entry(parent).or_default().push(key);
                }
            }
        }
        Ok(groups
            .into_iter()
            .map(|(parent, members)| {
                let keys: HashSet<&str> = members
                    .iter()
                    .filter_map(|member| keys_by_member.get(&guid_to_db(*member)))
                    .flatten()
                    .map(String::as_str)
                    .collect();
                (parent, i64::try_from(keys.len()).unwrap_or(i64::MAX))
            })
            .collect())
    }

    /// The `AggregateFolder`'s countable leaves and this user's played subset
    /// (`GetRecursiveChildCount`/`GetUnplayedCount` for the physical root).
    ///
    /// v12.2 reaches the aggregate's leaves through its `AncestorIds` closure
    /// (`GetPlayedAndTotalCountBatch`, `ItemCountService.cs:614-654`), which
    /// holds every library row but an extra: an extra has no parent, so its
    /// closure (`BaseItem.GetAncestorIds`) holds only its library's
    /// `CollectionFolder`. The closure holds neither the migration's
    /// `PLACEHOLDER` row, which holds detached `UserData`, nor the by-name rows
    /// (`Genre`, `MusicGenre`, `Studio`, `Person`, `Year`, a by-name
    /// `MusicArtist`), all stored `IsFolder = 0` with no `AncestorIds`. So a
    /// leaf counts here when it has no owner and its closure holds one of the
    /// `whole_server_roots!` (the aggregate, or a library's `CollectionFolder`
    /// for rows an older Ferrofin scan wrote). Counting every leaf row on the
    /// server instead served 36,552 on a Jellyfin 12.x adoption fixture whose
    /// libraries hold 12,659.
    ///
    /// The total arm is [`WHOLE_SERVER_TOTAL_COUNT_SQL`], the played arm
    /// [`WHOLE_SERVER_PLAYED_COUNT_SQL`]; read the latter's doc for why that
    /// count is `DISTINCT` and drives from `UserData`.
    async fn whole_server_leaf_counts(
        &self,
        user: &UserEntity,
        roots: RootFolderIds,
    ) -> Result<PlayedAndTotal, ServiceError> {
        let mut filter = InternalItemsQuery::default();
        filter.set_user(user.clone());
        let preferences = crate::query_restrictions::resolve(&self.db, &filter).await?;
        let filter = preferences.unwrap_or(filter);
        let (aggregate, user_root) = (guid_to_db(roots.aggregate), guid_to_db(roots.user_root));
        let mut total_query = restrict_count_query(
            sqlx::query(WHOLE_SERVER_TOTAL_COUNT_SQL)
                .bind(aggregate.as_str())
                .bind(user_root.as_str()),
            &filter,
            Some(r#"anc."ItemId""#),
        )?;
        let total: i64 = total_query
            .build_query_scalar()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        let mut played_query = restrict_count_query(
            sqlx::query(WHOLE_SERVER_PLAYED_COUNT_SQL)
                .bind(aggregate.as_str())
                .bind(user_root.as_str())
                .bind(user.id.as_str()),
            &filter,
            None,
        )?;
        let played: i64 = played_query
            .build_query_scalar()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(PlayedAndTotal {
            played: i32::try_from(played).unwrap_or(i32::MAX),
            total: i32::try_from(total).unwrap_or(i32::MAX),
        })
    }

    /// The `AggregateFolder`'s virtual children — the plug-in folders
    /// `LibraryManager.CreateRootFolder` parents to it and registers with
    /// `AddVirtualChild` (v12.2 LibraryManager.cs:1146, the playlists folder,
    /// the only call site).
    ///
    /// Type-narrow on purpose: the aggregate's other children (the physical
    /// library folders, `%AppDataPath%/collections`, the recordings folder) are
    /// not virtual children and must not be counted into the user root.
    async fn virtual_child_count(&self, aggregate: Uuid) -> Result<i64, ServiceError> {
        let count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems"
               WHERE "PrimaryVersionId" IS NULL AND "ParentId" = ?1 AND "Type" IN (?2, ?3)"#,
        )
        .bind(guid_to_db(aggregate))
        .bind(stored_type_name(BaseItemKind::PlaylistsFolder).unwrap_or_default())
        .bind(stored_type_name(BaseItemKind::ManualPlaylistsFolder).unwrap_or_default())
        .fetch_one(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(count)
    }
}

/// Rolls a `stored-type-name → count` map up into an [`ItemCounts`], mapping each
/// known type to its field (C# `GetItemCounts`); `item_count` is the grand total.
fn counts_from_type_map(by_type: &HashMap<String, i32>) -> ItemCounts {
    let get = |kind: BaseItemKind| -> i32 {
        stored_type_name(kind)
            .and_then(|name| by_type.get(name).copied())
            .unwrap_or(0)
    };
    ItemCounts {
        movie_count: get(BaseItemKind::Movie),
        series_count: get(BaseItemKind::Series),
        episode_count: get(BaseItemKind::Episode),
        artist_count: get(BaseItemKind::MusicArtist),
        program_count: get(BaseItemKind::LiveTvProgram),
        trailer_count: get(BaseItemKind::Trailer),
        song_count: get(BaseItemKind::Audio),
        album_count: get(BaseItemKind::MusicAlbum),
        music_video_count: get(BaseItemKind::MusicVideo),
        box_set_count: get(BaseItemKind::BoxSet),
        book_count: get(BaseItemKind::Book),
        item_count: by_type.values().sum(),
    }
}

/// The per-name credited-item count aggregate: `names` bound person names and,
/// when `types` is non-zero, that many stored `Type` names to restrict to.
///
/// The `CROSS JOIN`s pin the join order. SQLite reads `CROSS JOIN` as "do not
/// reorder"; the join semantics are exactly those of the inner joins it
/// replaces, so the rows are unchanged. Left to itself the planner drives from
/// `BaseItems` by `Type` — walking every movie/episode/series in the library and
/// only then filtering on the person name (12.9 ms on a 9.8k-item library).
/// Seeded from `Peoples` it seeks `IX_Peoples_Name`, then
/// `IX_PeopleBaseItemMap_PeopleId`, then the `BaseItems` id index, touching only
/// the credits of the names actually asked for (0.3 ms on the same library).
fn people_name_counts_sql(names: usize, types: usize) -> String {
    let mut sql = format!(
        r#"SELECT p."Name", bi."Type", COUNT(DISTINCT bi."Id")
           FROM "Peoples" p
           CROSS JOIN "PeopleBaseItemMap" pm ON pm."PeopleId" = p."Id"
           CROSS JOIN "BaseItems" bi ON bi."Id" = pm."ItemId"
           WHERE p."Name" IN ({})"#,
        placeholders(names)
    );
    if types > 0 {
        sql.push_str(r#" AND bi."Type" IN ("#);
        sql.push_str(&placeholders(types));
        sql.push(')');
    }
    sql.push_str(r#" GROUP BY p."Name", bi."Type""#);
    sql
}

/// Each page folder (the JSON array `?1`) with every folder sharing its
/// non-empty `PresentationUniqueKey`, itself among them: a primary-key probe
/// per page id, then an `IX_BaseItems_PresentationUniqueKey` seek. The `+`
/// keeps the planner off the `IsFolder`-led composites. `EXPLAIN QUERY PLAN`
/// pinned by `the_presentation_key_groups_seek_by_key`.
fn presentation_key_groups_sql() -> &'static str {
    r#"SELECT r."Id", m."Id" FROM json_each(?1) AS page
       CROSS JOIN "BaseItems" r ON r."Id" = page.value
       CROSS JOIN "BaseItems" m ON m."PresentationUniqueKey" = r."PresentationUniqueKey"
       WHERE r."PresentationUniqueKey" <> '' AND +m."IsFolder""#
}

/// The grouped leaf-descendant count for a page of folders. Its first bind is
/// the page's folder ids as one JSON array (`page`, read through `json_each`),
/// ahead of any `extra_where` bind; the statement also takes an optional extra
/// join and extra `WHERE` fragment (the played subset adds a `UserData` join).
///
/// A folder's leaves come from three branches, v12.2
/// `GetPlayedAndTotalCountBatch` (`ItemCountService.cs:597-690`): its
/// `AncestorIds` closure (`ancestorLeaves`), its `LinkedChildren`
/// (`linkedLeaves`), and the closure of each linked child that is a folder
/// (`linkedFolderLeaves`, a series in a collection). Box sets and playlists
/// have no closure of their own, so without the linked branches every one of
/// them counted 0/0 and read played. The branches are `UNION ALL`: the
/// `COUNT(DISTINCT …)` already counts a leaf once per folder however many
/// branches or playlist entries reach it, which is upstream's `Union` +
/// `Distinct`. Each branch seeds from the page through its own
/// `ParentItemId`/`ParentId` index, so a page with no linked children pays two
/// empty index probes per folder. The price of the compound is the
/// `GROUP BY`: it is no longer read off the driving index's order, so SQLite
/// sorts the branches' rows into a temp b-tree first.
///
/// The ids are one `json_each` bind, not an `IN (?, …)` list, because three
/// branches would bind such a list three times: a page past 10,922 folders
/// would then exceed SQLite's 32,766-variable limit and fail outright. As one
/// JSON value the page has no size limit, and no chunking is needed.
///
/// The leaf test is an `EXISTS`, not a `JOIN`, and that is load-bearing. As an
/// inner join, `bi."IsFolder" = 0` is a predicate SQLite may use to DRIVE the
/// query: with more than a handful of parents it picks
/// `IX_BaseItems_IsFolder_TopParentId_IsVirtualItem_PresentationUniqueKey_DateCreated
/// (IsFolder=?)` and walks EVERY non-folder row in the library (people
/// included), probing `AncestorIds` per row and sorting the result into a temp
/// b-tree for the `GROUP BY` — O(library), not O(descendants). An `EXISTS`
/// cannot be a driving term, so each branch stays seeded by the page and
/// `BaseItems` is reached by `Id=?`. `bi."Id"` is unique, so the inner join
/// could never multiply rows: the two forms are exactly equivalent.
///
/// Measured on the bench library (9,862 items / 6,795 ancestor edges) for the
/// 24-series page `/Items?includeItemTypes=Series`, alternating legs in
/// process: 23.5 ms → 1.96 ms p50, rows identical.
///
/// The played subset (`extra_join`) must arrive as a `CROSS JOIN`, for the same
/// reason and with the same effect. `ud."UserId" = ? AND ud."Played" = 1` is a
/// driving term too: with a plain `JOIN`, SQLite seeds from
/// `FerrofinIX_UserData_UserId_Played_ItemId (UserId=? AND Played=?)` — every
/// PLAYED row the user owns, at ANY parent count, one included — and probes the
/// closure per row per parent, O(the user's play history) rather than
/// O(descendants of these parents), plus a temp b-tree for the `GROUP BY`.
/// `CROSS JOIN` is SQLite's join-order pin: it forbids reordering and changes
/// nothing else, so the plan becomes the page's branches (`SCAN a`) →
/// `BaseItems (Id=?)` → that same Ferrofin index used the other way round,
/// `(UserId=? AND Played=? AND ItemId=?)`. The statement alone, on the adopted
/// bench database (93,818 ancestor edges, 4,886 played rows), unpinned → pinned: one season
/// 1.05 → 0.01 ms, the 16-album `/Items/Latest` page 9.86 → 0.10, a 24-series
/// page 14.11 → 0.63, a 100-album page 55.05 → 0.99. End to end, with two
/// servers interleaved request-for-request over 60 samples each: `/Items/Latest`
/// for music 11.33 → 2.13 ms p50, a 100-album page 57.98 → 7.88, `/Artists`
/// 58.98 → 11.57, a series' `/Seasons` 3.20 → 0.77; bodies byte-identical.
/// These figures, and the loss-regime ones below, were measured before the
/// linked branches, when the statement read `AncestorIds` alone and its
/// `GROUP BY` still fell out of the driving index's order.
///
/// The `COUNT(DISTINCT a."ItemId")` is load-bearing for CORRECTNESS, not for
/// the pin — do not delete it. (The pin needs nothing from it: a join's result
/// set is order-independent, so the two orders agree either way.) `UserData`'s
/// key is `(ItemId, UserId, CustomDataKey)`, so one `(item, user)` pair can
/// carry several rows — 2,154 of them on that same adopted database — and the
/// join multiplies: without the `DISTINCT` the shows folder counts 1,382 played
/// leaves against a true 691, and movies 2,649 against 883. The `DISTINCT` is
/// also v12.2's rule: `GetPlayedAndTotalCountBatch` counts
/// `g.Where(x => x.Played).Select(x => x.Id).Distinct()` (`:663-664`), and a
/// leaf is played when ANY of its key rows is, not all of them.
///
/// TODO: v12.2's played test is `DescendantQueryHelper.IsPlayedBy`, which also
/// counts a primary as played when one of its alternate versions is
/// (`LinkedChildren` of type `LocalAlternateVersion`/`LinkedAlternateVersion`
/// whose child the user played). Both played statements here, this one and
/// [`WHOLE_SERVER_PLAYED_COUNT_SQL`], read the leaf's own `UserData` only,
/// while the per-item linked-children path already counts the version group
/// (`push_is_played`). Un-defer path: join that version arm into both played
/// statements together, as `IsPlayedBy` does, so a page holding a folder and
/// the `AggregateFolder` keeps one played rule.
///
/// The pin has a loss regime, and it is bounded. It wins whenever the parents'
/// combined closure is smaller than the user's played-row count — every album /
/// artist / season / series page — and loses in the mirror case: the three
/// physical library folders (20,457 closure rows between them) measure 4.63 ms
/// unpinned against 19.18 ms pinned. Those folders DO reach here: the page's
/// folder list (`dto_service.rs`, the `get_played_and_total_count_batch` call
/// site) keeps only the folders whose `UserData` reads the counts (v12.2
/// `DtoService.cs:207-209`), which drops `CollectionFolder`, `UserView`, the
/// by-name kinds, and an album or artist page without `RecursiveItemCount`, but
/// a plain `Folder` passes. What bounds the loss is the `total` arm: it is this
/// same statement WITHOUT the `UserData` join, it always drives from the
/// page's branches, and on that page it cost 20.36 ms — already the same order.
/// So the pinned played arm took such a page from ~25 ms to ~40 ms, never from
/// milliseconds to tens of them, while every page that does NOT hold a
/// library-sized folder gets the 10-100× win above.
///
/// Merged alternate versions (`PrimaryVersionId` set) are hidden duplicates of
/// their primary — counting them inflated every series/season episode total
/// after a merge-versions pass. A stacked part or local version (owned, no
/// `ExtraType`) belongs to its owner and is no item of its own; an extra
/// is counted (`DescendantQueryHelper.IsDistinctLibraryItem`).
///
/// `IsVirtualItem = 0` is upstream's, not an optimisation: v12.2's batch keeps
/// only `DescendantQueryHelper.IsCountableLeaf` (`!b.IsFolder &&
/// !b.IsVirtualItem`, DescendantQueryHelper.cs:21-22), and
/// `Folder.GetRecursiveChildCount` (Folder.cs:977-991) sets
/// `IsVirtualItem = false`, so a missing episode is counted by neither. It was
/// absent here while the whole-server variant
/// (`whole_server_leaf_counts`, the `AggregateFolder`'s counts) carried it,
/// which meant a page holding both an ordinary folder and the aggregate counted
/// them by two different rules.
fn folder_leaf_count_sql(extra_join: &str, extra_where: &str) -> String {
    format!(
        r#"WITH page("Id") AS (SELECT value FROM json_each(?))
           SELECT a."ParentItemId", COUNT(DISTINCT a."ItemId")
           FROM (SELECT anc."ParentItemId", anc."ItemId" FROM "AncestorIds" anc
                  WHERE anc."ParentItemId" IN (SELECT "Id" FROM page)
                 UNION ALL
                 SELECT lc."ParentId", lc."ChildId" FROM "LinkedChildren" lc
                  WHERE lc."ParentId" IN (SELECT "Id" FROM page)
                 UNION ALL
                 SELECT lc."ParentId", linked."ItemId" FROM "LinkedChildren" lc
                   JOIN "AncestorIds" linked ON linked."ParentItemId" = lc."ChildId"
                  WHERE lc."ParentId" IN (SELECT "Id" FROM page)
                    AND EXISTS (SELECT 1 FROM "BaseItems" lf
                                WHERE lf."Id" = lc."ChildId" AND lf."IsFolder" = 1)) a{extra_join}
           WHERE EXISTS (SELECT 1 FROM "BaseItems" bi
                         WHERE bi."Id" = a."ItemId"
                           AND bi."IsFolder" = 0
                           AND bi."IsVirtualItem" = 0
                           AND bi."PrimaryVersionId" IS NULL
                           AND (bi."OwnerId" IS NULL OR bi."ExtraType" IS NOT NULL)){extra_where}
           GROUP BY a."ParentItemId""#
    )
}

/// The folders whose closures hold the `AggregateFolder`'s leaves, bound as
/// `(aggregate, user root)`: the aggregate itself, which a Jellyfin scan and
/// a current Ferrofin scan put in the closure of every library row but an
/// extra, and every folder directly under the user root, the libraries'
/// `CollectionFolder`s, for rows an older Ferrofin scan wrote, whose closure
/// stops there. (The aggregate's own children, the location folders, never
/// hold a row the aggregate does not.)
///
/// The head of both whole-server statements.
macro_rules! whole_server_roots {
    () => {
        r#"WITH roots("Id") AS (SELECT ? UNION SELECT r."Id" FROM "BaseItems" r WHERE r."ParentId" = ?)"#
    };
}

/// The whole-server played-leaf count — the `AggregateFolder`'s played arm,
/// bound to one `UserId`. The sibling of [`folder_leaf_count_sql`] for the one
/// parent whose closure is every library, so it drives from the played rows
/// rather than from a closure.
///
/// The `COUNT(DISTINCT bi."Id")` is v12.2's rule and it is load-bearing.
/// `ItemCountService.GetPlayedAndTotalCountFromQuery`
/// (v12.2 `ItemCountService.cs:735-749`) projects ONE boolean PER ITEM —
/// `query.Select(DescendantQueryHelper.IsPlayedBy(userId))`, whose first arm is
/// `b.UserData!.Any(u => u.UserId.Equals(userId) && u.Played)` — and counts the true
/// ones (its second, alternate-version arm is not ported here; see the TODO on
/// [`folder_leaf_count_sql`], which covers both played statements), and
/// `GetPlayedAndTotalCountBatch` (`:663-664`) spells the same thing out as
/// `g.Where(x => x.Played).Select(x => x.Id).Distinct().Count()`. `UserData`'s
/// primary key is `(ItemId, UserId, CustomDataKey)`, so one `(item, user)` pair
/// can carry several rows — 2,154 such pairs on the adopted bench database —
/// and a bare `COUNT(*)` over the join counts a leaf once per key row. It read
/// 4,031 played leaves against a true 1,574 for the `bench` user (855 against
/// 285 for `viewer`), out of 23,928 total (counted, before gl-226, over every
/// leaf row on the server rather than the libraries' closures).
///
/// It also restores an invariant the old form could break: the played
/// predicates are a strict superset of the `total` arm's, so `played <= total`
/// now always holds. Multiplied rows could push `played` ABOVE `total` and hand
/// a caller a negative `total - played`.
///
/// `DISTINCT` and not `EXISTS`, which is the opposite of the choice
/// [`folder_leaf_count_sql`] makes for its leaf test — because the driving side
/// is the opposite too. There the closure is small and `BaseItems` is the
/// library, so an `EXISTS` is what STOPS `IsFolder` from driving. Here no
/// closure drives: an `EXISTS` leaf test makes `bi."IsFolder" = 0` the only
/// driving term, so SQLite walks every leaf row on the server and probes
/// `UserData` per row (`SEARCH bi USING INDEX IX_BaseItems_IsFolder_… (IsFolder=?)` +
/// `CORRELATED SCALAR SUBQUERY`), measured 15.9 ms against the join's 1.6 on the
/// adopted bench database. The join keeps SQLite seeding from
/// `FerrofinIX_UserData_UserId_Played_ItemId (UserId=? AND Played=?)` — only the
/// rows this user actually played — and probing `BaseItems` by id, which is the
/// same plan the un-`DISTINCT` version had; the `DISTINCT` adds one temp b-tree
/// over those few thousand ids and 0.30 ms (`bench`) / 0.05 ms (`viewer`),
/// against a `total` arm that already costs ~11 ms on the same call. No join
/// order is pinned and no index is added: the planner already picks the played
/// rows as the driving side, and `whole_server_leaf_counts_seek_from_the_user`
/// pins that shape.
///
/// A leaf counts only with no owner and in the closure of one of the
/// `whole_server_roots!`: one `AncestorIds` primary-key probe per played
/// row, which leaves the driving side alone.
const WHOLE_SERVER_PLAYED_COUNT_SQL: &str = concat!(
    whole_server_roots!(),
    r#"
   SELECT COUNT(DISTINCT bi."Id") FROM "BaseItems" bi
   JOIN "UserData" ud ON ud."ItemId" = bi."Id"
   WHERE bi."IsFolder" = 0 AND bi."PrimaryVersionId" IS NULL
     AND bi."OwnerId" IS NULL
     AND bi."IsVirtualItem" = 0 AND ud."UserId" = ? AND ud."Played" = 1
     AND EXISTS (SELECT 1 FROM "AncestorIds" anc
                 WHERE anc."ItemId" = bi."Id" AND anc."ParentItemId" IN (SELECT "Id" FROM roots))"#
);

/// The whole-server leaf count, the `AggregateFolder`'s total arm: the
/// distinct countable leaves in the closures of the `whole_server_roots!`. It
/// seeds `AncestorIds` by `ParentItemId` per root and tests each row as a
/// leaf by primary key, the `EXISTS` keeping `BaseItems."IsFolder"` from
/// driving, as in [`folder_leaf_count_sql`]; `whole_server_leaf_counts_seek_from_the_roots`
/// pins it.
const WHOLE_SERVER_TOTAL_COUNT_SQL: &str = concat!(
    whole_server_roots!(),
    r#"
   SELECT COUNT(DISTINCT anc."ItemId") FROM "AncestorIds" anc
   WHERE anc."ParentItemId" IN (SELECT "Id" FROM roots)
     AND EXISTS (SELECT 1 FROM "BaseItems" bi
                 WHERE bi."Id" = anc."ItemId" AND bi."IsFolder" = 0
                   AND bi."PrimaryVersionId" IS NULL AND bi."IsVirtualItem" = 0
                   AND bi."OwnerId" IS NULL)"#
);

/// The per-clean-value count aggregate for the `ItemValues`-backed by-name kinds
/// (genre / music genre / studio / artist): `cleans` bound clean values,
/// `value_types` the `ItemValue.Type` discriminants that kind owns, and, when
/// `types` is non-zero, that many stored item `Type` names to restrict to.
///
/// The `iv."Type"` restriction is not optional and is not a perf hint. C#
/// `ItemCountService.GetItemCountsForNameItem` (`ItemCountService.cs:140-168`)
/// switches on the by-name kind and pins the value type per arm —
/// `ItemValueType.Studios` for a `Studio`, `.Genre` for `Genre`/`MusicGenre`,
/// `.Artist | .AlbumArtist` for a `MusicArtist`. Matching on `CleanValue` alone
/// makes a name collision a count collision: the by-name `Studio` "Action"
/// inherited every movie carrying the *genre* "Action" (measured 201 against
/// Jellyfin's 0). Ferrofin's own query translator already gets this right
/// (`translate_query.rs:1033-1052`); this aggregate was the one place that did not.
///
/// Same join-order pin as [`people_name_counts_sql`], for the same reason:
/// driven from `BaseItems` the planner walks every item of every counted type
/// before the clean-value filter can reject anything, where seeding from
/// `ItemValues` — one row per genre/studio/artist name — seeks straight to the
/// values asked for.
fn item_value_counts_sql(cleans: usize, types: usize, value_types: &[ItemValueType]) -> String {
    let mut sql = format!(
        r#"SELECT iv."CleanValue", bi."Type", COUNT(DISTINCT bi."Id")
           FROM "ItemValues" iv
           CROSS JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
           CROSS JOIN "BaseItems" bi ON bi."Id" = ivm."ItemId"
           WHERE iv."CleanValue" IN ({})"#,
        placeholders(cleans)
    );
    if !value_types.is_empty() {
        sql.push_str(r#" AND iv."Type" IN ("#);
        for (i, t) in value_types.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            // Bound discriminants, not user input — inlined so the binds stay
            // positional for the caller's two loops.
            sql.push_str(&i32::from(*t).to_string());
        }
        sql.push(')');
    }
    if types > 0 {
        sql.push_str(r#" AND bi."Type" IN ("#);
        sql.push_str(&placeholders(types));
        sql.push(')');
    }
    sql.push_str(r#" GROUP BY iv."CleanValue", bi."Type""#);
    sql
}

/// The `ItemValue.Type`s a by-name `kind` counts through, or [`None`] for a kind
/// the C# `switch` does not handle through `ItemValues`.
///
/// [`None`] is meaningful twice over: `Person` and `Year` have their own
/// branches, and every other kind hits the C# `default:` arm, which returns an
/// empty [`ItemCounts`] rather than counting something.
fn by_name_count_value_types(kind: BaseItemKind) -> Option<&'static [ItemValueType]> {
    match kind {
        BaseItemKind::Genre | BaseItemKind::MusicGenre => Some(&[ItemValueType::Genre]),
        BaseItemKind::Studio => Some(&[ItemValueType::Studios]),
        BaseItemKind::MusicArtist => Some(&[ItemValueType::Artist, ItemValueType::AlbumArtist]),
        _ => None,
    }
}

/// The per-year count aggregate: `years` bound production years and, when
/// `types` is non-zero, that many stored item `Type` names to restrict to.
///
/// C# `ItemCountService.cs:170-181` counts a `Year` by `e.ProductionYear == year`
/// — it never touches `ItemValues`, which is why routing a `Year` through the
/// clean-value aggregate answered 0 for every year that has content.
/// `IX_BaseItems_ProductionYear` is not part of the pinned Jellyfin schema, so
/// this is a scan of the type-filtered rows — the same shape the C# query
/// planner runs. A viewer's library scope is a residual check here too
/// ([`access_filtered_count_query`]): it would move the walk onto a
/// `(Type, TopParentId)` index, faster for a user with one library and slower
/// for one with all of them, and that trade is not one to take from a single
/// statement. `access_filtered_name_counts_keep_their_seeks` pins the walk.
fn production_year_counts_sql(years: usize, types: usize) -> String {
    let mut sql = format!(
        r#"SELECT bi."ProductionYear", bi."Type", COUNT(DISTINCT bi."Id")
           FROM "BaseItems" bi
           WHERE bi."ProductionYear" IN ({})"#,
        placeholders(years)
    );
    if types > 0 {
        sql.push_str(r#" AND bi."Type" IN ("#);
        sql.push_str(&placeholders(types));
        sql.push(')');
    }
    sql.push_str(r#" GROUP BY bi."ProductionYear", bi."Type""#);
    sql
}

/// Builds a `?, ?, …` placeholder list of length `n`.
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
    #[tokio::test]
    async fn extra_counts_batch_chunks_deduplicates_and_uses_owner_index() {
        let db = crate::test_support::test_db().await;
        let owner = Uuid::new_v4();
        crate::test_support::seed_named_item(&db, owner, BaseItemKind::Movie, "Movie").await;
        let extra = Uuid::new_v4();
        crate::test_support::seed_named_item(&db, extra, BaseItemKind::Video, "Clip").await;
        let mut row = crate::test_support::fetch_item(&db, extra).await;
        row.owner_id = Some(guid_to_db(owner));
        row.extra_type = Some(1);
        crate::test_support::save_item(&db, &row).await;
        let svc = FerrofinItemCountService::new(db.clone());
        assert!(svc.get_extra_counts_batch(&[]).await.unwrap().is_empty());
        let mut owners = vec![Uuid::new_v4(); ferrofin_db::BATCH_BIND_CHUNK + 1];
        owners[0] = owner;
        owners[ferrofin_db::BATCH_BIND_CHUNK] = owner;
        let counts = svc.get_extra_counts_batch(&owners).await.unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(
            counts[&owner],
            ExtraCounts {
                special_features: 1,
                local_trailers: 0
            }
        );
        let sql = format!("EXPLAIN QUERY PLAN {}", extra_counts_sql(1));
        let plan = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(sql))
            .bind(guid_to_db(owner))
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert!(
            plan.iter().any(|r| r
                .3
                .contains("SEARCH BaseItems USING INDEX IX_BaseItems_OwnerId")),
            "{plan:?}"
        );
        db.close().await;
        assert!(svc.get_extra_counts_batch(&[owner]).await.is_err());
    }

    use super::*;
    use crate::test_support::{
        fetch_item, seed_child_item, seed_folder_item, seed_item, seed_item_genre, seed_item_value,
        seed_item_with_data, seed_named_item, seed_user, seed_user_data, set_clean_name, test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use rstest::rstest;

    fn svc(db: &Database) -> FerrofinItemCountService {
        FerrofinItemCountService::new(db.clone())
    }

    /// The user root's `ChildCount` is its own children **plus** the
    /// `AggregateFolder`'s virtual children — C#
    /// `UserRootFolder.GetChildCount(user) => GetChildren(user, true).Count`
    /// over `GetEligibleChildrenForRecursiveChildren`, which concatenates
    /// `LibraryManager.RootFolder.VirtualChildren` (UserRootFolder.cs:84-102).
    #[tokio::test]
    async fn the_user_root_child_count_includes_the_aggregates_virtual_children() {
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let user_root = Uuid::from_u128(0xA001);
        let aggregate = Uuid::from_u128(0xA002);
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        seed_folder_item(
            &db,
            user_root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
            None,
        )
        .await;
        for (n, id) in [Uuid::from_u128(0xA010), Uuid::from_u128(0xA011)]
            .into_iter()
            .enumerate()
        {
            seed_folder_item(
                &db,
                id,
                BaseItemKind::CollectionFolder,
                &format!("Library {n}"),
                Some(user_root),
            )
            .await;
        }
        // The one virtual child `CreateRootFolder` registers…
        seed_folder_item(
            &db,
            Uuid::from_u128(0xA020),
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(aggregate),
        )
        .await;
        // …and a physical library folder, which is a plain child of the
        // aggregate and must NOT be counted into the user root.
        seed_folder_item(
            &db,
            Uuid::from_u128(0xA021),
            BaseItemKind::Folder,
            "movies",
            Some(aggregate),
        )
        .await;

        let roots = RootFolderIds {
            user_root,
            aggregate,
        };
        let counts = svc(&db)
            .with_root_ids(roots)
            .get_child_count_batch(&[user_root, aggregate], None)
            .await
            .expect("counts");

        assert_eq!(
            counts.get(&user_root).copied(),
            Some(3),
            "2 libraries + the playlists folder, never the physical folder"
        );
        assert_eq!(
            counts.get(&aggregate).copied(),
            Some(3),
            "`Folder.GetChildren` delegates the physical root to the user root"
        );
    }

    /// Without the root ids wired the service behaves exactly as before.
    #[tokio::test]
    async fn the_concat_is_inert_until_the_root_ids_are_injected() {
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let user_root = Uuid::from_u128(0xB001);
        let aggregate = Uuid::from_u128(0xB002);
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        seed_folder_item(&db, user_root, BaseItemKind::UserRootFolder, "MF", None).await;
        seed_folder_item(
            &db,
            Uuid::from_u128(0xB020),
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(aggregate),
        )
        .await;

        let counts = svc(&db)
            .get_child_count_batch(&[user_root], None)
            .await
            .expect("counts");

        assert_eq!(counts.get(&user_root).copied(), Some(0));
    }

    /// Reads the stored rows for `ids`, the way the DTO projection already has
    /// them in hand when it asks for counts.
    async fn stored_rows(db: &Database, ids: &[Uuid]) -> Vec<BaseItemEntity> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(fetch_item(db, *id).await);
        }
        out
    }

    /// The count-service view of those rows — `Name`/`CleanName` exactly as
    /// stored, never a test-side guess at what the cleaner produced.
    fn name_rows(rows: &[BaseItemEntity]) -> Vec<NameItemRow<'_>> {
        rows.iter()
            .map(|row| NameItemRow {
                id: Uuid::parse_str(&row.id).expect("row id"),
                name: row.name.as_deref(),
                clean_name: row.clean_name.as_deref(),
            })
            .collect()
    }

    /// Seeds a folder with a played and an unplayed child, wiring the AncestorIds
    /// closure and a user with UserData for the played child. Returns
    /// `(folder, played_child, unplayed_child, user_entity)`.
    async fn seed_folder_tree(db: &Database) -> (Uuid, Uuid, Uuid, UserEntity) {
        let folder = Uuid::from_u128(0xF001);
        seed_item(db, folder, BaseItemKind::Folder).await;
        let played = Uuid::from_u128(0xF002);
        seed_item(db, played, BaseItemKind::Movie).await;
        let unplayed = Uuid::from_u128(0xF003);
        seed_item(db, unplayed, BaseItemKind::Movie).await;

        for child in [played, unplayed] {
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(guid_to_db(child))
                .bind(guid_to_db(folder))
                .execute(db.writer())
                .await
                .expect("ancestor");
        }

        let user_id = Uuid::from_u128(0xF00D);
        let user = seed_user(db, user_id).await;
        seed_user_data(db, user_id, played, true, None).await;
        (folder, played, unplayed, user)
    }

    #[tokio::test]
    async fn parental_rating_applies_to_counts_before_aggregation() {
        use ferrofin_traits::persistence::LinkedChildrenService;
        let db = test_db().await;
        let service = svc(&db);
        let (folder, adult, child, mut user) = seed_folder_tree(&db).await;
        user.max_parental_rating_score = Some(12);
        sqlx::query(r#"UPDATE "Users" SET "MaxParentalRatingScore" = 12 WHERE "Id" = ?"#)
            .bind(&user.id)
            .execute(db.writer())
            .await
            .unwrap();
        for (id, rating) in [(adult, 18), (child, 10)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = ?, "ParentId" = ?, "ProductionYear" = 2020 WHERE "Id" = ?"#)
                .bind(rating).bind(guid_to_db(folder)).bind(guid_to_db(id)).execute(db.writer()).await.unwrap();
            seed_item_genre(&db, id, "Shared").await;
        }
        let mut filter = InternalItemsQuery::default();
        filter.set_user(user.clone());
        let batch = service
            .get_played_and_total_count_batch(&[folder], &user)
            .await
            .unwrap();
        assert_eq!((batch[&folder].played, batch[&folder].total), (0, 1));
        assert_eq!(
            service
                .get_child_count_batch(&[folder], filter.user_id())
                .await
                .unwrap()[&folder],
            1
        );
        // The folder is a library's: directly under the user root.
        seed_roots(&db).await;
        under_user_root(&db, folder).await;
        let whole = service
            .whole_server_leaf_counts(&user, ROOTS)
            .await
            .unwrap();
        let mut unrestricted_user = user.clone();
        unrestricted_user.max_parental_rating_score = None;
        let unrestricted = service
            .whole_server_leaf_counts(&unrestricted_user, ROOTS)
            .await
            .unwrap();
        assert_eq!(unrestricted.total, 2, "the folder's two leaves");
        assert_eq!(whole.played, 0);
        assert_eq!(whole.total, unrestricted.total - 1);
        // A by-name item's counts cover the libraries its viewer may open
        // (`AddUserToQuery`), so the rated pair goes in one this user can.
        crate::test_support::seed_library_over(&db, &[adult, child]).await;
        crate::test_support::grant_default_permissions(&db, Uuid::parse_str(&user.id).unwrap())
            .await;
        for (kind, name) in [
            (BaseItemKind::Genre, "Shared"),
            (BaseItemKind::Year, "2020"),
        ] {
            let id = Uuid::new_v4();
            seed_named_item(&db, id, kind, name).await;
            set_clean_name(&db, id, &name.to_lowercase()).await;
            let counts = service
                .get_item_counts_for_name_item(kind, id, &[BaseItemKind::Movie], &filter)
                .await
                .unwrap();
            assert_eq!(counts.movie_count, 1, "{kind:?}");
        }
        let collection = Uuid::new_v4();
        seed_item(&db, collection, BaseItemKind::BoxSet).await;
        let links = crate::FerrofinLinkedChildrenService::new(db.clone());
        links
            .upsert_linked_child(collection, adult, 0)
            .await
            .unwrap();
        assert_eq!(
            service
                .get_child_count_batch(&[collection], filter.user_id())
                .await
                .unwrap()[&collection],
            0
        );
        links
            .upsert_linked_child(collection, child, 0)
            .await
            .unwrap();
        assert_eq!(
            service
                .get_child_count_batch(&[collection], filter.user_id())
                .await
                .unwrap()[&collection],
            1
        );
        // The linked branches are access-filtered too: the blocked (and
        // played) member drops out of the collection's played/total.
        let batch = service
            .get_played_and_total_count_batch(&[collection], &user)
            .await
            .unwrap();
        assert_eq!(
            (batch[&collection].played, batch[&collection].total),
            (0, 1)
        );
    }

    #[tokio::test]
    async fn played_and_total_over_descendant_closure() {
        let db = test_db().await;
        let service = svc(&db);
        let (folder, _played, _unplayed, user) = seed_folder_tree(&db).await;

        let filter = InternalItemsQuery {
            recursive: true,
            user: Some(user),
            ..Default::default()
        };

        let both = service
            .get_played_and_total_count(&filter, folder)
            .await
            .expect("played/total");
        assert_eq!(both.total, 2);
        assert_eq!(both.played, 1);

        assert_eq!(
            service
                .get_total_count(&filter, folder)
                .await
                .expect("total"),
            2
        );
        assert_eq!(
            service
                .get_played_count(&filter, folder)
                .await
                .expect("played"),
            1
        );

        // No user on the filter → played is 0 but total still counts.
        let no_user = InternalItemsQuery {
            recursive: true,
            ..Default::default()
        };
        let anon = service
            .get_played_and_total_count(&no_user, folder)
            .await
            .expect("anon");
        assert_eq!(anon.total, 2);
        assert_eq!(anon.played, 0);

        // An ancestor with no descendants → default zeros.
        let empty = service
            .get_played_and_total_count(&no_user, Uuid::from_u128(0xDEAD))
            .await
            .expect("empty");
        assert_eq!(empty.total, 0);
    }

    /// v12.2 `BuildGroupedDescendantsQuery` counts every folder sharing the
    /// requested one's `PresentationUniqueKey` (a series merged across two
    /// libraries); a folder with a key of its own counts alone.
    #[tokio::test]
    async fn played_and_total_sums_the_presentation_key_group() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x5740_0001);
        let user = seed_user(&db, user_id).await;
        let first = Uuid::from_u128(0x5740_1001);
        let second = Uuid::from_u128(0x5740_1002);
        let other = Uuid::from_u128(0x5740_1003);
        for (series, key) in [(first, "merged"), (second, "merged"), (other, "own")] {
            crate::test_support::seed_folder_item(&db, series, BaseItemKind::Series, "Show", None)
                .await;
            sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = ?1 WHERE "Id" = ?2"#)
                .bind(key)
                .bind(guid_to_db(series))
                .execute(db.writer())
                .await
                .expect("key");
            let episode = Uuid::new_v4();
            seed_item(&db, episode, BaseItemKind::Episode).await;
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(guid_to_db(episode))
                .bind(guid_to_db(series))
                .execute(db.writer())
                .await
                .expect("ancestor");
            seed_user_data(&db, user_id, episode, series == first, None).await;
        }
        let mut filter = InternalItemsQuery::default();
        filter.set_user(user.clone());
        let service = svc(&db);
        // The page's batch sums each group too (v12.2
        // `GetPlayedAndTotalCountBatch`), one statement for the groups.
        let batch = service
            .get_played_and_total_count_batch(&[first, second, other], &user)
            .await
            .expect("batch");
        // Either merged row stands for the whole series.
        for (series, (played, total)) in [(first, (1, 2)), (second, (1, 2)), (other, (0, 1))] {
            let b = batch[&series];
            assert_eq!((b.played, b.total), (played, total), "batch");
            let c = service
                .get_played_and_total_count(&filter, series)
                .await
                .expect("played/total");
            assert_eq!((c.played, c.total), (played, total));
            assert_eq!(
                service.get_played_count(&filter, series).await.unwrap(),
                played
            );
            assert_eq!(
                service.get_total_count(&filter, series).await.unwrap(),
                total
            );
        }
    }

    /// Sets one `BaseItems` column of `id` for a child-count fixture.
    async fn set_column(db: &Database, id: Uuid, column: &str, value: Option<String>) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"UPDATE "BaseItems" SET "{column}" = ?1 WHERE "Id" = ?2"#
        )))
        .bind(value)
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("set column");
    }

    /// v12.2 `ItemCountServiceTests.SeedSeries`: a series, its season and two
    /// episodes tied to the season by `SeasonId`. `flat` files the episodes
    /// under the series, as a season-less folder layout does.
    async fn seed_series(db: &Database, flat: bool, virtual_episodes: bool) -> (Uuid, Uuid) {
        let series = Uuid::new_v4();
        let season = Uuid::new_v4();
        // Folders, as upstream's `CreateItem` makes them.
        seed_folder_item(db, series, BaseItemKind::Series, "Show", None).await;
        seed_folder_item(db, season, BaseItemKind::Season, "Season 1", Some(series)).await;
        for name in ["e1", "e2"] {
            let episode = Uuid::new_v4();
            let parent = if flat { series } else { season };
            seed_child_item(db, episode, BaseItemKind::Episode, name, parent).await;
            set_column(db, episode, "SeasonId", Some(guid_to_db(season))).await;
            if virtual_episodes {
                set_column(db, episode, "IsVirtualItem", Some("1".to_owned())).await;
            }
        }
        (series, season)
    }

    /// `GetChildCountBatch_FlatSeriesStructure_CountsEpisodesUnderTheirSeason`:
    /// "The series holds the season, not the episodes: counting those here
    /// would double them up."
    #[tokio::test]
    async fn a_flat_series_counts_its_episodes_under_their_season() {
        let db = test_db().await;
        let (series, season) = seed_series(&db, true, false).await;
        let counts = svc(&db)
            .get_child_count_batch(&[series, season], None)
            .await
            .expect("child counts");
        assert_eq!((counts[&series], counts[&season]), (1, 2));
    }

    /// `GetChildCountBatch_SeasonFolderStructure_CountsEachEpisodeOnce`.
    #[tokio::test]
    async fn a_season_folder_counts_each_episode_once() {
        let db = test_db().await;
        let (series, season) = seed_series(&db, false, false).await;
        let counts = svc(&db)
            .get_child_count_batch(&[series, season], None)
            .await
            .expect("child counts");
        assert_eq!((counts[&series], counts[&season]), (1, 2));
    }

    /// `GetChildCountBatch_MissingEpisodes_CountedUnlessTheUserHidesThem` and
    /// `GetChildCountBatch_NoUser_CountsMissingEpisodes`.
    #[tokio::test]
    async fn missing_episodes_count_unless_the_user_hides_them() {
        let db = test_db().await;
        let (_, season) = seed_series(&db, false, true).await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let service = svc(&db);
        let count = |user| {
            let service = service.clone();
            async move {
                service
                    .get_child_count_batch(&[season], user)
                    .await
                    .expect("child counts")[&season]
            }
        };
        for (shown, expected) in [(true, 2), (false, 0)] {
            sqlx::query(r#"UPDATE "Users" SET "DisplayMissingEpisodes" = ?1 WHERE "Id" = ?2"#)
                .bind(shown)
                .bind(guid_to_db(user))
                .execute(db.writer())
                .await
                .expect("user");
            assert_eq!(count(Some(user)).await, expected, "shown {shown}");
        }
        assert_eq!(count(None).await, 2, "no user counts them");
    }

    /// `GetChildCountBatch_MergedFolders_CountsDistinctChildKeys`: each
    /// library's row of a merged series holds a "Season 1", and those two are
    /// one season to the user.
    #[tokio::test]
    async fn merged_folders_count_their_distinct_child_keys() {
        let db = test_db().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        for series in [a, b] {
            // A presentation-key group is made of folders.
            seed_folder_item(&db, series, BaseItemKind::Series, "Show", None).await;
            set_column(
                &db,
                series,
                "PresentationUniqueKey",
                Some("merged-series".into()),
            )
            .await;
        }
        for (parent, key) in [
            (a, "merged-series-001"),
            (b, "merged-series-001"),
            (b, "merged-series-002"),
        ] {
            let season = Uuid::new_v4();
            seed_folder_item(&db, season, BaseItemKind::Season, "Season", Some(parent)).await;
            set_column(&db, season, "PresentationUniqueKey", Some(key.into())).await;
        }
        let counts = svc(&db)
            .get_child_count_batch(&[a, b], None)
            .await
            .expect("child counts");
        assert_eq!((counts[&a], counts[&b]), (2, 2));
    }

    /// `GetMergedChildCounts` reads a merged season's episodes by `SeasonId`
    /// (here filed flat under each library's series row). The same episode
    /// in both libraries counts once, a key-less one by its id, and the
    /// access and missing-episode rules apply to the group's children; a
    /// season in a group of one keeps the plain count.
    #[tokio::test]
    async fn a_merged_season_counts_its_episodes_once() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        sqlx::query(
            r#"UPDATE "Users" SET "DisplayMissingEpisodes" = 0, "MaxParentalRatingScore" = 12
               WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(user))
        .execute(db.writer())
        .await
        .expect("user");
        let rate = |id, rating: i32| {
            set_column(
                &db,
                id,
                "InheritedParentalRatingValue",
                Some(rating.to_string()),
            )
        };
        let (series_a, series_b) = (Uuid::new_v4(), Uuid::new_v4());
        let (season_a, season_b, own) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for series in [series_a, series_b] {
            seed_folder_item(&db, series, BaseItemKind::Series, "Show", None).await;
            set_column(&db, series, "PresentationUniqueKey", Some("show".into())).await;
            rate(series, 10).await;
        }
        for (season, series, key) in [
            (season_a, series_a, "show-001"),
            (season_b, series_b, "show-001"),
            (own, series_b, "show-002"),
        ] {
            seed_folder_item(&db, season, BaseItemKind::Season, "Season", Some(series)).await;
            set_column(&db, season, "PresentationUniqueKey", Some(key.into())).await;
            rate(season, 10).await;
        }
        // (series filed under, season, key, virtual, rating)
        for (series, season, key, missing, rating) in [
            (series_a, season_a, Some("ep1"), false, 10),
            (series_b, season_b, Some("ep1"), false, 10),
            (series_b, season_b, None, false, 10),
            (series_a, season_a, Some("ep4"), true, 10),
            (series_a, season_a, Some("ep5"), false, 18),
            (series_b, own, Some("ep6"), false, 10),
        ] {
            let episode = Uuid::new_v4();
            seed_child_item(&db, episode, BaseItemKind::Episode, "e", series).await;
            set_column(&db, episode, "SeasonId", Some(guid_to_db(season))).await;
            set_column(
                &db,
                episode,
                "PresentationUniqueKey",
                key.map(str::to_owned),
            )
            .await;
            set_column(
                &db,
                episode,
                "IsVirtualItem",
                Some(u8::from(missing).to_string()),
            )
            .await;
            rate(episode, rating).await;
        }
        let service = svc(&db);
        let page = [series_a, series_b, season_a, season_b, own];
        let counts = service
            .get_child_count_batch(&page, None)
            .await
            .expect("child counts");
        assert_eq!(page.map(|id| counts[&id]), [2, 2, 4, 4, 1], "no user");
        // The user hides missing episodes and may not open the 18.
        let counts = service
            .get_child_count_batch(&page, Some(user))
            .await
            .expect("child counts");
        assert_eq!(
            page.map(|id| counts[&id]),
            [2, 2, 2, 2, 1],
            "restricted user"
        );
    }

    /// Since 12.1 a linked child counts only when it is an item the user may
    /// open (`GetChildCountBatch`'s `accessibleItems` join): a box set that
    /// links a film and that film's alternate version in the same library
    /// holds one title, and an owned non-extra is no child of it either.
    #[tokio::test]
    async fn a_linked_alternate_version_does_not_count() {
        use ferrofin_traits::persistence::LinkedChildrenService;
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let (library, collection) = (Uuid::new_v4(), Uuid::new_v4());
        seed_folder_item(&db, library, BaseItemKind::CollectionFolder, "Movies", None).await;
        seed_item(&db, collection, BaseItemKind::BoxSet).await;
        let [primary, alternate, owned] = [(); 3].map(|()| Uuid::new_v4());
        for id in [primary, alternate, owned] {
            seed_child_item(&db, id, BaseItemKind::Movie, "m", library).await;
            set_column(&db, id, "TopParentId", Some(guid_to_db(library))).await;
        }
        set_column(
            &db,
            alternate,
            "PrimaryVersionId",
            Some(guid_to_db(primary)),
        )
        .await;
        set_column(&db, owned, "OwnerId", Some(guid_to_db(primary))).await;
        let links = crate::FerrofinLinkedChildrenService::new(db.clone());
        for (order, id) in [primary, alternate, owned].into_iter().enumerate() {
            links
                .upsert_linked_child(collection, id, i32::try_from(order).unwrap_or(0))
                .await
                .expect("link");
        }
        let service = svc(&db);
        for viewer in [None, Some(user)] {
            let counts = service
                .get_child_count_batch(&[collection], viewer)
                .await
                .expect("child counts");
            assert_eq!(counts[&collection], 1, "{viewer:?}");
        }
    }

    /// With a user, an alternate version is hidden only while its primary is
    /// in the same library (`ApplyAlternateVersionFiltering`, the rule the
    /// browse applies): one whose primary is in another library is a child
    /// the user sees, so it counts. With no user, `IsDistinctLibraryItem`
    /// still drops every alternate.
    #[tokio::test]
    async fn a_cross_library_alternate_counts_for_a_user() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let (films, four_k, folder) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        seed_folder_item(&db, films, BaseItemKind::CollectionFolder, "Films", None).await;
        seed_folder_item(&db, four_k, BaseItemKind::CollectionFolder, "4K", None).await;
        seed_folder_item(&db, folder, BaseItemKind::Folder, "Dune", Some(four_k)).await;
        let (primary, alternate) = (Uuid::new_v4(), Uuid::new_v4());
        seed_child_item(&db, primary, BaseItemKind::Movie, "Dune", films).await;
        set_column(&db, primary, "TopParentId", Some(guid_to_db(films))).await;
        seed_child_item(&db, alternate, BaseItemKind::Movie, "Dune", folder).await;
        set_column(&db, alternate, "TopParentId", Some(guid_to_db(four_k))).await;
        set_column(
            &db,
            alternate,
            "PrimaryVersionId",
            Some(guid_to_db(primary)),
        )
        .await;
        let service = svc(&db);
        let count = |viewer| {
            let service = service.clone();
            async move {
                service
                    .get_child_count_batch(&[folder], viewer)
                    .await
                    .expect("child counts")[&folder]
            }
        };
        assert_eq!(count(Some(user)).await, 1, "a user sees the 4K copy");
        assert_eq!(count(None).await, 0, "no user collapses every alternate");
    }

    /// `GetChildCountBatch_NoUser_StillCollapsesAlternateVersions`: with no
    /// user, the alternate version and an owned non-extra are not the
    /// folder's children; the primary and an extra are.
    #[tokio::test]
    async fn with_no_user_alternate_versions_still_collapse() {
        let db = test_db().await;
        let folder = Uuid::new_v4();
        seed_named_item(&db, folder, BaseItemKind::Folder, "Movies").await;
        let [primary, alternate, extra, owned] = [(); 4].map(|()| Uuid::new_v4());
        for id in [primary, alternate, extra, owned] {
            seed_child_item(&db, id, BaseItemKind::Movie, "m", folder).await;
        }
        set_column(
            &db,
            alternate,
            "PrimaryVersionId",
            Some(guid_to_db(primary)),
        )
        .await;
        for id in [extra, owned] {
            set_column(&db, id, "OwnerId", Some(guid_to_db(primary))).await;
        }
        set_column(&db, extra, "ExtraType", Some("2".into())).await;
        let counts = svc(&db)
            .get_child_count_batch(&[folder], None)
            .await
            .expect("child counts");
        assert_eq!(counts[&folder], 2);
    }

    /// A Jellyfin library's `ChildCount` is its physical folders' children.
    ///
    /// Nothing carries a `CollectionFolder`'s id as `ParentId` on an adopted
    /// database, so grouping on the raw column reports 0 while the very same
    /// browse returns the items — the two would disagree in one response.
    /// Until a Ferrofin scan saves the row, that is: then it hangs off the
    /// collection folder itself, and the library counts both kinds.
    #[tokio::test]
    async fn a_jellyfin_library_counts_its_physical_folder_s_children() {
        let db = test_db().await;
        let service = svc(&db);
        let view = Uuid::from_u128(0xE101);
        let physical = Uuid::from_u128(0xE102);
        let plain = Uuid::from_u128(0xE105);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::CollectionFolder,
            "TV",
            &format!(r#"{{"PhysicalFolderIds":["{}"]}}"#, physical.simple()),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "TV").await;
        seed_child_item(
            &db,
            Uuid::from_u128(0xE103),
            BaseItemKind::Episode,
            "a",
            physical,
        )
        .await;
        seed_child_item(
            &db,
            Uuid::from_u128(0xE104),
            BaseItemKind::Episode,
            "b",
            physical,
        )
        .await;
        // A row a Ferrofin scan saved: under the collection folder itself.
        seed_child_item(
            &db,
            Uuid::from_u128(0xE107),
            BaseItemKind::Episode,
            "c",
            view,
        )
        .await;
        // An ordinary folder alongside it, to prove the translation is scoped.
        seed_named_item(&db, plain, BaseItemKind::Folder, "Other").await;
        seed_child_item(
            &db,
            Uuid::from_u128(0xE106),
            BaseItemKind::Movie,
            "m",
            plain,
        )
        .await;

        let counts = service
            .get_child_count_batch(&[view, plain], None)
            .await
            .expect("child counts");
        assert_eq!(
            counts[&view], 3,
            "the view counts through its folder, and its own children"
        );
        assert_eq!(counts[&plain], 1, "an ordinary parent counts its own");
    }

    /// `Folder.LinkedChildren.Length` — the RAW edge count, with none of
    /// `get_child_count_batch`'s hierarchical fallback. A parent with no linked
    /// children is ABSENT from the map, which is what lets the DTO shortcut
    /// honour upstream's `if (folderChildCount > 0)` guard instead of writing a
    /// spurious `ChildCount: 0`.
    #[tokio::test]
    async fn linked_children_count_batch_counts_only_edges() {
        let db = test_db().await;
        let service = svc(&db);
        let playlist = Uuid::from_u128(0xE101);
        let season = Uuid::from_u128(0xE102);
        seed_item(&db, playlist, BaseItemKind::Playlist).await;
        seed_item(&db, season, BaseItemKind::Season).await;
        for n in [0xE103_u128, 0xE104] {
            let child = Uuid::from_u128(n);
            seed_item(&db, child, BaseItemKind::Audio).await;
            sqlx::query(
                r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
                   VALUES (?1, (SELECT COALESCE(MAX("SortOrder"), -1) + 1 FROM "LinkedChildren" WHERE "ParentId" = ?1), ?2, 0)"#,
            )
            .bind(guid_to_db(playlist))
            .bind(guid_to_db(child))
            .execute(db.writer())
            .await
            .expect("link child");
        }
        // A hierarchical child of the season, which must NOT be counted here.
        let episode = Uuid::from_u128(0xE105);
        seed_item(&db, episode, BaseItemKind::Episode).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(episode))
            .bind(guid_to_db(season))
            .execute(db.writer())
            .await
            .expect("set parent");

        let counts = service
            .get_linked_children_count_batch(&[playlist, season])
            .await
            .expect("linked counts");
        assert_eq!(Some(&2), counts.get(&playlist));
        assert_eq!(None, counts.get(&season), "an empty array is an ABSENT key");
        assert!(
            service
                .get_linked_children_count_batch(&[])
                .await
                .expect("empty")
                .is_empty()
        );
    }

    /// An unpaged browse can put more folders on one page than SQLite's
    /// default bind limit (32,766); both count batches must still answer.
    #[tokio::test]
    async fn count_batches_answer_pages_past_the_sqlite_bind_limit() {
        let db = test_db().await;
        let service = svc(&db);
        let playlist = Uuid::from_u128(0xE201);
        let child = Uuid::from_u128(0xE202);
        let season = Uuid::from_u128(0xE203);
        let episode = Uuid::from_u128(0xE204);
        seed_item(&db, playlist, BaseItemKind::Playlist).await;
        seed_item(&db, child, BaseItemKind::Audio).await;
        seed_item(&db, season, BaseItemKind::Season).await;
        seed_item(&db, episode, BaseItemKind::Episode).await;
        sqlx::query(
            r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
               VALUES (?1, 0, ?2, 0)"#,
        )
        .bind(guid_to_db(playlist))
        .bind(guid_to_db(child))
        .execute(db.writer())
        .await
        .expect("link child");
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(episode))
            .bind(guid_to_db(season))
            .execute(db.writer())
            .await
            .expect("set parent");
        // The counted parents sit in the first chunk and in a middle one, so
        // every chunk's rows must survive the merge, not just the last's.
        let mut page: Vec<Uuid> = (1..=33_000_u128).map(Uuid::from_u128).collect();
        page[0] = playlist;
        page[ferrofin_db::BATCH_BIND_CHUNK * 32] = season;

        let linked = service
            .get_linked_children_count_batch(&page)
            .await
            .expect("linked counts for a huge page");
        assert_eq!(linked.get(&playlist), Some(&1));
        let children = service
            .get_child_count_batch(&page, None)
            .await
            .expect("child counts for a huge page");
        assert_eq!(children.get(&playlist), Some(&1), "linked arm, first chunk");
        assert_eq!(
            children.get(&season),
            Some(&1),
            "hierarchical arm, middle chunk"
        );
        assert_eq!(children.len(), page.len());
    }

    #[tokio::test]
    async fn played_total_batch_and_child_count_batch() {
        let db = test_db().await;
        let service = svc(&db);
        let (folder, _p, _u, user_entity) = seed_folder_tree(&db).await;

        // Direct-parent children for the child-count batch.
        let parent = Uuid::from_u128(0xE001);
        seed_item(&db, parent, BaseItemKind::Folder).await;
        let kid = Uuid::from_u128(0xE002);
        seed_item(&db, kid, BaseItemKind::Movie).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(kid))
            .bind(guid_to_db(parent))
            .execute(db.writer())
            .await
            .expect("set parent");

        // Batch over the closure folder AND `parent` (which has a direct child via
        // ParentId but no `AncestorIds` closure row — like a by-name item): the
        // grouped query resolves both in one pass, closure folder = 2/1, the
        // closure-less folder is left out as v12.2 leaves a 0/0 out
        // (`ItemCountService.cs:684`), for the caller to count per item.
        let batch = service
            .get_played_and_total_count_batch(&[folder, parent], &user_entity)
            .await
            .expect("batch");
        assert_eq!(batch[&folder].total, 2);
        assert_eq!(batch[&folder].played, 1);
        assert!(!batch.contains_key(&parent));

        let counts = service
            .get_child_count_batch(&[parent, folder], None)
            .await
            .expect("child counts");
        assert_eq!(counts[&parent], 1);
        assert_eq!(counts[&folder], 0);

        // A parent with linked children reports those instead of its
        // hierarchical children (C# `linkedCount > 0 ? linkedCount : ...`).
        let boxset = Uuid::from_u128(0xE003);
        seed_item(&db, boxset, BaseItemKind::BoxSet).await;
        for (n, linked) in [(0xE004_u128, true), (0xE005, true), (0xE006, false)] {
            let child = Uuid::from_u128(n);
            seed_item(&db, child, BaseItemKind::Movie).await;
            if linked {
                sqlx::query(
                    r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
                   VALUES (?1, (SELECT COALESCE(MAX("SortOrder"), -1) + 1 FROM "LinkedChildren" WHERE "ParentId" = ?1), ?2, 0)"#,
                )
                .bind(guid_to_db(boxset))
                .bind(guid_to_db(child))
                .execute(db.writer())
                .await
                .expect("link child");
            } else {
                sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
                    .bind(guid_to_db(child))
                    .bind(guid_to_db(boxset))
                    .execute(db.writer())
                    .await
                    .expect("set parent");
            }
        }
        let counts = service
            .get_child_count_batch(&[boxset], None)
            .await
            .expect("linked child counts");
        assert_eq!(counts[&boxset], 2);
    }

    /// A stacked movie is one item (`DescendantQueryHelper.
    /// IsDistinctLibraryItem`): its second part — owned by the primary, no
    /// `ExtraType`, no `PrimaryVersionId` — is neither a child of its folder
    /// nor a leaf of its own, in a folder's counts or the whole server's, so
    /// the folder's unplayed badge clears once the primary is played. An
    /// extra still counts in its folder, but not on the whole server: an
    /// extra has no parent upstream, so the aggregate's closure never holds it.
    #[tokio::test]
    async fn a_stacked_part_counts_as_no_item_of_its_own() {
        let db = test_db().await;
        let service = svc(&db);
        let folder = Uuid::from_u128(0x5F01);
        let (primary, part, extra) = (
            Uuid::from_u128(0x5F02),
            Uuid::from_u128(0x5F03),
            Uuid::from_u128(0x5F04),
        );
        seed_item(&db, folder, BaseItemKind::Folder).await;
        seed_item(&db, primary, BaseItemKind::Movie).await;
        seed_item(&db, part, BaseItemKind::Video).await;
        seed_item(&db, extra, BaseItemKind::Trailer).await;
        for (child, parent, owner, extra_type) in [
            (primary, Some(folder), None, None),
            (part, Some(folder), Some(primary), None),
            (extra, None, Some(primary), Some(2)),
        ] {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "ParentId" = ?2, "OwnerId" = ?3, "ExtraType" = ?4
                   WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(child))
            .bind(parent.map(guid_to_db))
            .bind(owner.map(guid_to_db))
            .bind(extra_type)
            .execute(db.writer())
            .await
            .expect("shape");
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(guid_to_db(child))
                .bind(guid_to_db(folder))
                .execute(db.writer())
                .await
                .expect("ancestor");
        }
        let user_id = Uuid::from_u128(0x5F0D);
        let user = seed_user(&db, user_id).await;
        // The folder is a library's: directly under the user root.
        seed_roots(&db).await;
        under_user_root(&db, folder).await;

        let children = service
            .get_child_count_batch(&[folder], None)
            .await
            .expect("child counts");
        assert_eq!(children[&folder], 1, "the primary; the part is no child");
        let before = service
            .get_played_and_total_count_batch(&[folder], &user)
            .await
            .expect("batch");
        assert_eq!((before[&folder].played, before[&folder].total), (0, 2));
        let server = service
            .whole_server_leaf_counts(&user, ROOTS)
            .await
            .expect("server");
        let leaves = server.total;
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = NULL WHERE "Id" = ?1"#)
            .bind(guid_to_db(part))
            .execute(db.writer())
            .await
            .expect("unown");
        let unowned = service
            .whole_server_leaf_counts(&user, ROOTS)
            .await
            .expect("server");
        assert_eq!(unowned.total, leaves + 1, "an unowned part is a leaf");
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(part))
            .bind(guid_to_db(primary))
            .execute(db.writer())
            .await
            .expect("own");

        seed_user_data(&db, user_id, primary, true, None).await;
        seed_user_data(&db, user_id, extra, true, None).await;
        let after = service
            .get_played_and_total_count_batch(&[folder], &user)
            .await
            .expect("batch");
        assert_eq!(
            (after[&folder].played, after[&folder].total),
            (2, 2),
            "nothing left unplayed"
        );
        let server = service
            .whole_server_leaf_counts(&user, ROOTS)
            .await
            .expect("server");
        assert_eq!(
            (server.played, server.total),
            (1, leaves),
            "the primary only"
        );
    }

    #[tokio::test]
    async fn played_total_from_linked_children() {
        let db = test_db().await;
        let service = svc(&db);

        let parent = Uuid::from_u128(0xAA01);
        seed_item(&db, parent, BaseItemKind::BoxSet).await;
        let played = Uuid::from_u128(0xAA02);
        seed_item(&db, played, BaseItemKind::Movie).await;
        let unplayed = Uuid::from_u128(0xAA03);
        seed_item(&db, unplayed, BaseItemKind::Movie).await;

        for child in [played, unplayed] {
            sqlx::query(
                r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
                   VALUES (?1, (SELECT COALESCE(MAX("SortOrder"), -1) + 1 FROM "LinkedChildren" WHERE "ParentId" = ?1), ?2, 0)"#,
            )
            .bind(guid_to_db(parent))
            .bind(guid_to_db(child))
            .execute(db.writer())
            .await
            .expect("linked child");
        }

        let user = Uuid::from_u128(0xAA0D);
        let user_entity = seed_user(&db, user).await;
        seed_user_data(&db, user, played, true, None).await;

        let filter = InternalItemsQuery {
            user: Some(user_entity),
            ..Default::default()
        };
        let res = service
            .get_played_and_total_count_from_linked_children(&filter, parent)
            .await
            .expect("linked");
        assert_eq!(res.total, 2);
        assert_eq!(res.played, 1);

        // Anonymous filter → played 0, total counts the linked children.
        let anon = service
            .get_played_and_total_count_from_linked_children(&InternalItemsQuery::default(), parent)
            .await
            .expect("anon linked");
        assert_eq!(anon.total, 2);
        assert_eq!(anon.played, 0);

        // A parent with no linked children → default zeros.
        let none = service
            .get_played_and_total_count_from_linked_children(
                &InternalItemsQuery::default(),
                Uuid::from_u128(0xBAD0),
            )
            .await
            .expect("no linked");
        assert_eq!(none.total, 0);
    }

    /// `GetPlayedAndTotalCountFromLinkedChildren` descends through a linked
    /// folder (a series in a collection counts its episodes, not itself),
    /// leaves virtual items out (`IsCountableLeaf`), and does not scope to the
    /// user's libraries: a member in a library the user cannot open still
    /// counts, as upstream's `InternalItemsQuery(user)` names no top parents.
    #[tokio::test]
    async fn linked_children_count_descends_through_linked_folders() {
        use ferrofin_traits::persistence::{
            ItemPersistenceService as _, LinkedChildrenService as _,
        };
        let db = test_db().await;
        let service = svc(&db);
        let (boxset, movie, series, episode, missing) = (
            Uuid::from_u128(0xAB01),
            Uuid::from_u128(0xAB02),
            Uuid::from_u128(0xAB03),
            Uuid::from_u128(0xAB04),
            Uuid::from_u128(0xAB05),
        );
        seed_item(&db, boxset, BaseItemKind::BoxSet).await;
        seed_item(&db, movie, BaseItemKind::Movie).await;
        crate::test_support::seed_folder_item(&db, series, BaseItemKind::Series, "Show", None)
            .await;
        crate::test_support::seed_episode(&db, episode, "show", 1, 1, false, None).await;
        crate::test_support::seed_episode(&db, missing, "show", 1, 2, true, None).await;
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        for child in [episode, missing] {
            persistence.set_ancestors(child, &[series]).await.unwrap();
        }
        // The user may open no library at all.
        crate::test_support::seed_library_over(&db, &[movie, episode, missing]).await;
        let links = crate::FerrofinLinkedChildrenService::new(db.clone());
        for child in [movie, series] {
            links.upsert_linked_child(boxset, child, 0).await.unwrap();
        }
        let user = Uuid::from_u128(0xAB0D);
        let user_entity = seed_user(&db, user).await;
        seed_user_data(&db, user, episode, true, None).await;

        let res = service
            .get_played_and_total_count_from_linked_children(
                &InternalItemsQuery {
                    user: Some(user_entity),
                    ..Default::default()
                },
                boxset,
            )
            .await
            .expect("linked");
        assert_eq!(
            (res.played, res.total),
            (1, 2),
            "the movie and the aired episode"
        );
    }

    /// A `Year` is not an `ItemValues` row, so the CleanName path counted zero
    /// for it and every `/Years/{year}` reported `ChildCount: 0`. C#
    /// `SetItemByNameInfo` counts a Year by `query.Years = [year]` — the
    /// production-year column.
    #[tokio::test]
    async fn item_counts_for_a_year_count_by_production_year() {
        let db = test_db().await;
        let service = svc(&db);

        let year = Uuid::from_u128(0xCC01);
        seed_named_item(&db, year, BaseItemKind::Year, "2020").await;
        set_clean_name(&db, year, "2020").await;

        for (n, kind, production_year) in [
            (0xCC02_u128, BaseItemKind::Movie, 2020),
            (0xCC03, BaseItemKind::Movie, 2020),
            (0xCC04, BaseItemKind::Series, 2020),
            // A different year must not be counted.
            (0xCC05, BaseItemKind::Movie, 2021),
        ] {
            let id = Uuid::from_u128(n);
            seed_named_item(&db, id, kind, &format!("Item {n}")).await;
            sqlx::query(r#"UPDATE "BaseItems" SET "ProductionYear" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(i64::from(production_year))
                .execute(db.writer())
                .await
                .expect("set production year");
        }

        let counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Year,
                year,
                &[BaseItemKind::Movie, BaseItemKind::Series],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("year counts");
        assert_eq!(counts.movie_count, 2);
        assert_eq!(counts.series_count, 1);
        assert_eq!(counts.item_count, 3);
    }

    /// C# guards the Year arm with `when int.TryParse(dto.Name, …)`; a Year row
    /// whose name is not an integer falls through to `default: return;` and
    /// keeps unset (zero) counts.
    #[tokio::test]
    async fn item_counts_for_a_non_numeric_year_are_zero() {
        let db = test_db().await;
        let service = svc(&db);
        let year = Uuid::from_u128(0xCD01);
        seed_named_item(&db, year, BaseItemKind::Year, "not-a-year").await;
        set_clean_name(&db, year, "not-a-year").await;

        let counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Year,
                year,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("counts");
        assert_eq!(counts, ItemCounts::default());
    }

    /// The batch form must give each year its own counts, not one year's to all.
    #[tokio::test]
    async fn item_counts_for_several_years_are_per_year() {
        let db = test_db().await;
        let service = svc(&db);
        let y2020 = Uuid::from_u128(0xCE01);
        let y2021 = Uuid::from_u128(0xCE02);
        seed_named_item(&db, y2020, BaseItemKind::Year, "2020").await;
        seed_named_item(&db, y2021, BaseItemKind::Year, "2021").await;
        for (n, production_year) in [(0xCE03_u128, 2020), (0xCE04, 2021), (0xCE05, 2021)] {
            let id = Uuid::from_u128(n);
            seed_named_item(&db, id, BaseItemKind::Movie, &format!("M{n}")).await;
            sqlx::query(r#"UPDATE "BaseItems" SET "ProductionYear" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(i64::from(production_year))
                .execute(db.writer())
                .await
                .expect("set production year");
        }

        let rows = stored_rows(&db, &[y2020, y2021]).await;
        let counts = service
            .get_item_counts_for_name_items(
                BaseItemKind::Year,
                &name_rows(&rows),
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("batch year counts");
        assert_eq!(counts[&y2020].movie_count, 1);
        assert_eq!(counts[&y2021].movie_count, 2);
    }

    /// The `ItemValue.Type` restriction, name-collision edition — the C#
    /// `switch` in `ItemCountService.cs:140-168` pins the value type per
    /// by-name kind, and dropping it makes a Studio inherit a Genre's items.
    /// Measured live before the fix: `/Studios/Action` reported ChildCount 201
    /// against Jellyfin's 0, because 200 movies carried the *genre* "Action".
    #[tokio::test]
    async fn a_by_name_studio_does_not_inherit_a_genre_of_the_same_name() {
        let db = test_db().await;
        let service = svc(&db);

        // One name, two different ItemValue types on two different items.
        let studio = Uuid::from_u128(0xCC01);
        seed_named_item(&db, studio, BaseItemKind::Studio, "Action").await;
        set_clean_name(&db, studio, "Action").await;
        let genre = Uuid::from_u128(0xCC02);
        seed_named_item(&db, genre, BaseItemKind::Genre, "Action").await;
        set_clean_name(&db, genre, "Action").await;

        let genre_movie = Uuid::from_u128(0xCC03);
        seed_named_item(&db, genre_movie, BaseItemKind::Movie, "GM").await;
        seed_item_genre(&db, genre_movie, "Action").await;
        let studio_movie = Uuid::from_u128(0xCC04);
        seed_named_item(&db, studio_movie, BaseItemKind::Movie, "SM").await;
        seed_item_value(
            &db,
            studio_movie,
            ferrofin_db::enums::ItemValueType::Studios,
            "Action",
        )
        .await;

        let studio_counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Studio,
                studio,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("studio counts");
        assert_eq!(
            studio_counts.movie_count, 1,
            "a Studio counts only ItemValueType::Studios rows"
        );

        let genre_counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Genre,
                genre,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("genre counts");
        assert_eq!(
            genre_counts.movie_count, 1,
            "a Genre counts only ItemValueType::Genre rows"
        );
    }

    /// A `MusicArtist` counts through `Artist | AlbumArtist` and nothing else —
    /// the C# `MusicArtist` arm. A same-named Genre value must not leak in.
    #[tokio::test]
    async fn a_by_name_artist_counts_only_artist_values() {
        let db = test_db().await;
        let service = svc(&db);

        let artist = Uuid::from_u128(0xCD01);
        seed_named_item(&db, artist, BaseItemKind::MusicArtist, "Rock").await;
        set_clean_name(&db, artist, "Rock").await;

        let track = Uuid::from_u128(0xCD02);
        seed_named_item(&db, track, BaseItemKind::Audio, "T").await;
        seed_item_value(
            &db,
            track,
            ferrofin_db::enums::ItemValueType::AlbumArtist,
            "Rock",
        )
        .await;
        // A DIFFERENT track carries "Rock" only as a genre.
        let other = Uuid::from_u128(0xCD03);
        seed_named_item(&db, other, BaseItemKind::Audio, "O").await;
        seed_item_genre(&db, other, "Rock").await;

        let counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::MusicArtist,
                artist,
                &[BaseItemKind::Audio],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("artist counts");
        assert_eq!(counts.song_count, 1, "the genre-only track must not count");
    }

    /// A `Year` counts by `ProductionYear`, not through `ItemValues`
    /// (`ItemCountService.cs:170-181`). Routing it through the clean-value
    /// aggregate answered 0 for every year that has content — measured live,
    /// `/Years/2020` reported ChildCount 0 against Jellyfin's 500.
    #[tokio::test]
    async fn a_by_name_year_counts_by_production_year() {
        let db = test_db().await;
        let service = svc(&db);

        let year = Uuid::from_u128(0xCE01);
        seed_named_item(&db, year, BaseItemKind::Year, "2020").await;
        set_clean_name(&db, year, "2020").await;

        for (raw, name, produced) in [
            (0xCE02_u128, "A", 2020),
            (0xCE03, "B", 2020),
            (0xCE04, "C", 2021),
        ] {
            let item = Uuid::from_u128(raw);
            seed_named_item(&db, item, BaseItemKind::Movie, name).await;
            sqlx::query(r#"UPDATE "BaseItems" SET "ProductionYear" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(item))
                .bind(produced)
                .execute(db.writer())
                .await
                .expect("set production year");
        }

        let counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Year,
                year,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("year counts");
        assert_eq!(counts.movie_count, 2, "only the 2020 movies count");

        // A Name that does not parse as a year counts zero (the C# `else` arm).
        let junk = Uuid::from_u128(0xCE09);
        seed_named_item(&db, junk, BaseItemKind::Year, "not-a-year").await;
        set_clean_name(&db, junk, "not-a-year").await;
        let zero = service
            .get_item_counts_for_name_item(
                BaseItemKind::Year,
                junk,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("junk year counts");
        assert_eq!(zero.movie_count, 0);
    }

    /// The C# `switch`'s `default:` arm returns an empty `ItemCounts` — a kind
    /// it does not handle is never counted by an unrestricted name match.
    #[tokio::test]
    async fn an_unhandled_by_name_kind_counts_nothing() {
        let db = test_db().await;
        let service = svc(&db);

        let tag = Uuid::from_u128(0xCF01);
        seed_named_item(&db, tag, BaseItemKind::Folder, "Action").await;
        set_clean_name(&db, tag, "Action").await;
        let movie = Uuid::from_u128(0xCF02);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;
        seed_item_genre(&db, movie, "Action").await;

        let counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Folder,
                tag,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("unhandled kind counts");
        assert_eq!(counts.movie_count, 0);
        assert_eq!(counts.item_count, 0);
    }

    #[tokio::test]
    async fn item_counts_for_name_item_groups_by_type() {
        let db = test_db().await;
        let service = svc(&db);

        // A by-name Genre "Drama" referenced by a movie and a series.
        let genre = Uuid::from_u128(0xBB01);
        seed_named_item(&db, genre, BaseItemKind::Genre, "Drama").await;
        set_clean_name(&db, genre, "Drama").await;

        let movie = Uuid::from_u128(0xBB02);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;
        seed_item_genre(&db, movie, "Drama").await;
        let series = Uuid::from_u128(0xBB03);
        seed_named_item(&db, series, BaseItemKind::Series, "S").await;
        seed_item_genre(&db, series, "Drama").await;

        let counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Genre,
                genre,
                &[BaseItemKind::Movie, BaseItemKind::Series],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("name-item counts");
        assert_eq!(counts.movie_count, 1);
        assert_eq!(counts.series_count, 1);
        assert_eq!(counts.item_count, 2);

        // Restricting related kinds to Movie only drops the series.
        let movies_only = service
            .get_item_counts_for_name_item(
                BaseItemKind::Genre,
                genre,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("movies only");
        assert_eq!(movies_only.movie_count, 1);
        assert_eq!(movies_only.series_count, 0);

        // A by-name item with no CleanName → default zeros (early return).
        let no_clean = Uuid::from_u128(0xBB09);
        seed_named_item(&db, no_clean, BaseItemKind::Genre, "").await;
        let zero = service
            .get_item_counts_for_name_item(
                BaseItemKind::Genre,
                no_clean,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("no clean");
        assert_eq!(zero.item_count, 0);
    }

    #[tokio::test]
    async fn item_counts_for_name_items_batches_a_page() {
        let db = test_db().await;
        let service = svc(&db);

        // Two genres with distinct related sets, plus one with no CleanName.
        let drama = Uuid::from_u128(0xDD01);
        seed_named_item(&db, drama, BaseItemKind::Genre, "Drama").await;
        set_clean_name(&db, drama, "Drama").await;
        let comedy = Uuid::from_u128(0xDD02);
        seed_named_item(&db, comedy, BaseItemKind::Genre, "Comedy").await;
        set_clean_name(&db, comedy, "Comedy").await;
        let no_clean = Uuid::from_u128(0xDD03);
        seed_named_item(&db, no_clean, BaseItemKind::Genre, "").await;

        let m1 = Uuid::from_u128(0xDD11);
        seed_named_item(&db, m1, BaseItemKind::Movie, "M1").await;
        seed_item_genre(&db, m1, "Drama").await;
        let m2 = Uuid::from_u128(0xDD12);
        seed_named_item(&db, m2, BaseItemKind::Movie, "M2").await;
        seed_item_genre(&db, m2, "Comedy").await;
        let s1 = Uuid::from_u128(0xDD13);
        seed_named_item(&db, s1, BaseItemKind::Series, "S1").await;
        seed_item_genre(&db, s1, "Comedy").await;

        let stored = stored_rows(&db, &[drama, comedy, no_clean]).await;
        let batch = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &name_rows(&stored),
                &[BaseItemKind::Movie, BaseItemKind::Series],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("batch counts");

        assert_eq!(batch[&drama].movie_count, 1);
        assert_eq!(batch[&drama].series_count, 0);
        assert_eq!(batch[&comedy].movie_count, 1);
        assert_eq!(batch[&comedy].series_count, 1);
        // A row without a CleanName still reports (zeros), matching the
        // per-item form's default.
        assert_eq!(batch[&no_clean].item_count, 0);

        // The per-item form (single = batch of one) agrees.
        let single = service
            .get_item_counts_for_name_item(
                BaseItemKind::Genre,
                comedy,
                &[BaseItemKind::Movie, BaseItemKind::Series],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("single counts");
        assert_eq!(single.movie_count, batch[&comedy].movie_count);
        assert_eq!(single.series_count, batch[&comedy].series_count);
    }

    /// The by-name count paths dedupe names before binding them. Guards the
    /// three edges that dedupe can break: an empty id list (which must never
    /// reach the SQL builder), several distinct by-name rows sharing one
    /// name/clean value (dedupe must collapse the *bind*, never the *result*),
    /// and an empty related-kind list (no `Type IN (…)` clause at all).
    #[tokio::test]
    async fn name_item_counts_dedupe_names_without_losing_rows() {
        let db = test_db().await;
        let service = svc(&db);

        // Empty id list → empty map, and no query is built at all.
        let empty = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &[],
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("empty ids");
        assert!(empty.is_empty(), "empty input yields empty output");
        let empty_people = service
            .get_item_counts_for_name_items(
                BaseItemKind::Person,
                &[],
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("empty person ids");
        assert!(empty_people.is_empty());

        // Three separate Genre by-name rows that all clean to "drama" — case
        // and diacritics are exactly what `GetCleanValue` folds — plus one that
        // doesn't. Dedupe binds "drama" once; all three rows must still report
        // the movie.
        let dupes = [
            (Uuid::from_u128(0x0D01), "Drama"),
            (Uuid::from_u128(0x0D02), "drama"),
            (Uuid::from_u128(0x0D03), "Dráma"),
        ];
        for (id, raw) in dupes {
            seed_named_item(&db, id, BaseItemKind::Genre, raw).await;
            set_clean_name(&db, id, raw).await;
        }
        let other = Uuid::from_u128(0x0D04);
        seed_named_item(&db, other, BaseItemKind::Genre, "Comedy").await;
        set_clean_name(&db, other, "Comedy").await;

        let movie = Uuid::from_u128(0x0D11);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;
        seed_item_genre(&db, movie, "Drama").await;
        let comedy_series = Uuid::from_u128(0x0D12);
        seed_named_item(&db, comedy_series, BaseItemKind::Series, "S").await;
        seed_item_genre(&db, comedy_series, "Comedy").await;

        let ids: Vec<Uuid> = dupes.iter().map(|(id, _)| *id).chain([other]).collect();
        let stored = stored_rows(&db, &ids).await;
        let rows = name_rows(&stored);
        let counts = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &rows,
                &[BaseItemKind::Movie, BaseItemKind::Series],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("duplicate-clean counts");
        assert_eq!(counts.len(), 4, "every id reports");
        for (id, raw) in dupes {
            assert_eq!(counts[&id].movie_count, 1, "{raw} counts the movie");
            assert_eq!(counts[&id].series_count, 0, "{raw} excludes the comedy");
        }
        assert_eq!(counts[&other].series_count, 1);
        assert_eq!(counts[&other].movie_count, 0);

        // No related-kind restriction → the `Type IN (…)` clause is omitted and
        // every type is counted.
        let unrestricted = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &rows,
                &[],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("unrestricted counts");
        assert_eq!(unrestricted[&dupes[0].0].movie_count, 1);
        assert_eq!(unrestricted[&other].series_count, 1);
    }

    /// The batch counts key off the names the CALLER supplies, never a re-read
    /// of the rows — the caller is projecting those rows already, and the
    /// re-read was a round trip per page on every by-name list endpoint.
    ///
    /// Proven by supplying rows whose name columns are absent while the stored
    /// rows have perfectly good ones: a service that went back to `BaseItems`
    /// would find the names and report non-zero counts.
    #[tokio::test]
    async fn name_counts_key_off_the_supplied_rows_not_a_re_read() {
        let db = test_db().await;
        let service = svc(&db);

        let drama = Uuid::from_u128(0xEE01);
        seed_named_item(&db, drama, BaseItemKind::Genre, "Drama").await;
        set_clean_name(&db, drama, "Drama").await;
        let movie = Uuid::from_u128(0xEE11);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;
        seed_item_genre(&db, movie, "Drama").await;

        let person = Uuid::from_u128(0xEE02);
        seed_named_item(&db, person, BaseItemKind::Person, "Ada Lovelace").await;
        let people_id = Uuid::from_u128(0xEE03);
        sqlx::query(r#"INSERT INTO "Peoples" ("Id","Name","PersonType") VALUES (?1,?2,?3)"#)
            .bind(guid_to_db(people_id))
            .bind("Ada Lovelace")
            .bind("Actor")
            .execute(db.writer())
            .await
            .expect("seed people");
        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap" ("ItemId","PeopleId","Role","ListOrder","SortOrder")
               VALUES (?1,?2,?3,0,0)"#,
        )
        .bind(guid_to_db(movie))
        .bind(guid_to_db(people_id))
        .bind("Role")
        .execute(db.writer())
        .await
        .expect("seed people map");

        // The stored rows do carry the names…
        let stored = stored_rows(&db, &[drama]).await;
        let counted = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &name_rows(&stored),
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("genre counts");
        assert_eq!(counted[&drama].movie_count, 1);

        // …but a row handed over without them must report zeros, for both the
        // ItemValues branch and the Peoples branch.
        let blank_genre = NameItemRow {
            id: drama,
            name: None,
            clean_name: None,
        };
        let blind = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                std::slice::from_ref(&blank_genre),
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("genre counts without a name");
        assert_eq!(
            blind[&drama].movie_count, 0,
            "the service re-read the row instead of using what it was given"
        );

        let blank_person = NameItemRow {
            id: person,
            name: None,
            clean_name: None,
        };
        let blind_person = service
            .get_item_counts_for_name_items(
                BaseItemKind::Person,
                std::slice::from_ref(&blank_person),
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("person counts without a name");
        assert_eq!(blind_person[&person].movie_count, 0);

        // The per-item form takes only an id, so it still reads the row itself
        // and must keep counting.
        let single = service
            .get_item_counts_for_name_item(
                BaseItemKind::Person,
                person,
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("single person counts");
        assert_eq!(single.movie_count, 1);
    }

    /// The Person branch dedupes on the raw `Name`, so two Person rows sharing a
    /// name must both receive the shared filmography, and a name carrying SQL
    /// metacharacters/quotes/unicode must round-trip as a bound parameter.
    #[tokio::test]
    async fn person_counts_dedupe_names_and_survive_quoting() {
        let db = test_db().await;
        let service = svc(&db);

        // One `Peoples` row; two Person by-name rows carrying the same Name.
        let raw_name = r#"O'Brien, "Bo%_" Ünïcode"#;
        let people_id = Uuid::from_u128(0x0AA0);
        sqlx::query(r#"INSERT INTO "Peoples" ("Id","Name","PersonType") VALUES (?1,?2,?3)"#)
            .bind(guid_to_db(people_id))
            .bind(raw_name)
            .bind("Actor")
            .execute(db.writer())
            .await
            .expect("seed people");

        // A decoy person whose name shares a LIKE-wildcard prefix — it must not
        // leak into the first person's counts (proving `?` binds literally).
        let decoy_people = Uuid::from_u128(0x0AA1);
        sqlx::query(r#"INSERT INTO "Peoples" ("Id","Name","PersonType") VALUES (?1,?2,?3)"#)
            .bind(guid_to_db(decoy_people))
            .bind(r#"O'Brien, "Bozo" Ünïcode"#)
            .bind("Actor")
            .execute(db.writer())
            .await
            .expect("seed decoy people");

        for (i, kind) in [
            BaseItemKind::Movie,
            BaseItemKind::Movie,
            BaseItemKind::Series,
        ]
        .iter()
        .enumerate()
        {
            let item = Uuid::from_u128(0x0AB0 + i as u128);
            seed_named_item(&db, item, *kind, "credit").await;
            sqlx::query(
                r#"INSERT INTO "PeopleBaseItemMap" ("ItemId","PeopleId","Role","ListOrder","SortOrder")
                   VALUES (?1,?2,?3,0,0)"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(people_id))
            .bind("Role")
            .execute(db.writer())
            .await
            .expect("seed people map");
        }
        // The decoy is credited on a third movie that must stay uncounted.
        let decoy_movie = Uuid::from_u128(0x0AC0);
        seed_named_item(&db, decoy_movie, BaseItemKind::Movie, "decoy credit").await;
        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap" ("ItemId","PeopleId","Role","ListOrder","SortOrder")
               VALUES (?1,?2,?3,0,0)"#,
        )
        .bind(guid_to_db(decoy_movie))
        .bind(guid_to_db(decoy_people))
        .bind("Role")
        .execute(db.writer())
        .await
        .expect("seed decoy map");

        let p1 = Uuid::from_u128(0x0AD1);
        let p2 = Uuid::from_u128(0x0AD2);
        for id in [p1, p2] {
            seed_named_item(&db, id, BaseItemKind::Person, raw_name).await;
        }

        let stored = stored_rows(&db, &[p1, p2]).await;
        let counts = service
            .get_item_counts_for_name_items(
                BaseItemKind::Person,
                &name_rows(&stored),
                &[BaseItemKind::Movie, BaseItemKind::Series],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("person counts");
        assert_eq!(counts.len(), 2);
        for id in [p1, p2] {
            assert_eq!(counts[&id].movie_count, 2, "decoy credit excluded");
            assert_eq!(counts[&id].series_count, 1);
            assert_eq!(counts[&id].item_count, 3);
        }
    }

    /// The by-name count path chunks its `IN` list at 500. Seeding 501 distinct
    /// by-name rows crosses that boundary in the count query
    /// (`distinct_cleans.chunks`); an off-by-one there silently drops the
    /// tail's counts.
    #[tokio::test]
    async fn name_item_counts_span_the_chunk_boundary() {
        /// One past the 500-item `IN`-chunk size used by the by-name count path.
        const N: u128 = 501;

        let db = test_db().await;
        let service = svc(&db);

        let mut genre_ids = Vec::with_capacity(N as usize);
        for i in 0..N {
            let name = format!("Genre{i}");
            let genre = Uuid::from_u128(0x10_0000 + i);
            seed_named_item(&db, genre, BaseItemKind::Genre, &name).await;
            set_clean_name(&db, genre, &name).await;
            genre_ids.push(genre);

            let movie = Uuid::from_u128(0x20_0000 + i);
            seed_named_item(&db, movie, BaseItemKind::Movie, &name).await;
            seed_item_genre(&db, movie, &name).await;
        }

        let stored = stored_rows(&db, &genre_ids).await;
        let counts = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &name_rows(&stored),
                &[BaseItemKind::Movie],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("chunked counts");

        assert_eq!(counts.len(), N as usize);
        for (i, id) in genre_ids.iter().enumerate() {
            assert_eq!(counts[id].movie_count, 1, "genre #{i} counted");
        }
        // Explicitly pin the elements straddling the 500-item chunk edge.
        assert_eq!(counts[&genre_ids[499]].movie_count, 1);
        assert_eq!(counts[&genre_ids[500]].movie_count, 1);
    }

    #[tokio::test]
    async fn get_item_counts_totals_every_matched_type() {
        let db = test_db().await;
        let service = svc(&db);

        // ItemCountService.GetItemCounts (v12.2, :73): ItemCount is the sum of
        // every type group, including types with no field of their own (Season).
        seed_item(&db, Uuid::from_u128(0xCC01), BaseItemKind::Movie).await;
        seed_item(&db, Uuid::from_u128(0xCC02), BaseItemKind::Movie).await;
        seed_item(&db, Uuid::from_u128(0xCC03), BaseItemKind::Series).await;
        seed_item(&db, Uuid::from_u128(0xCC04), BaseItemKind::Season).await;

        let counts = service
            .get_item_counts(&InternalItemsQuery::default())
            .await
            .expect("item counts");

        assert_eq!(counts.movie_count, 2);
        assert_eq!(counts.series_count, 1);
        assert_eq!(counts.item_count, 4);
    }

    #[tokio::test]
    async fn person_counts_credited_items_via_people_map() {
        let db = test_db().await;
        let service = svc(&db);

        // A Person by-name item, credited on two movies + a series through the
        // People map. People live outside ItemValues, so the CleanName path counts
        // zero — the Person branch must join PeopleBaseItemMap → Peoples on Name.
        let person = Uuid::from_u128(0xCC01);
        seed_named_item(&db, person, BaseItemKind::Person, "Alice Parity").await;
        let people_id = Uuid::from_u128(0xCC0A);
        sqlx::query(r#"INSERT INTO "Peoples" ("Id","Name","PersonType") VALUES (?1,?2,?3)"#)
            .bind(guid_to_db(people_id))
            .bind("Alice Parity")
            .bind("Actor")
            .execute(db.writer())
            .await
            .expect("seed people");

        for (i, kind) in [
            BaseItemKind::Movie,
            BaseItemKind::Movie,
            BaseItemKind::Series,
        ]
        .iter()
        .enumerate()
        {
            let item = Uuid::from_u128(0xCC10 + i as u128);
            seed_named_item(&db, item, *kind, "credit").await;
            sqlx::query(
                r#"INSERT INTO "PeopleBaseItemMap" ("ItemId","PeopleId","Role","ListOrder","SortOrder")
                   VALUES (?1,?2,?3,?4,?5)"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(people_id))
            .bind("Role")
            .bind(0)
            .bind(0)
            .execute(db.writer())
            .await
            .expect("seed people map");
        }

        let counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Person,
                person,
                &[BaseItemKind::Movie, BaseItemKind::Series],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("person counts");
        assert_eq!(counts.movie_count, 2, "both movie credits counted");
        assert_eq!(counts.series_count, 1);
        assert_eq!(counts.item_count, 3);
        sqlx::query(r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = 18 WHERE "Id" = ?"#)
            .bind(guid_to_db(Uuid::from_u128(0xCC10)))
            .execute(db.writer())
            .await
            .unwrap();
        // The viewer's counts cover the libraries they may open.
        crate::test_support::seed_library_over(&db, &[0xCC10, 0xCC11, 0xCC12].map(Uuid::from_u128))
            .await;
        let mut user = crate::test_support::seed_user_with_defaults(&db, Uuid::new_v4()).await;
        user.max_parental_rating_score = Some(12);
        let filter = InternalItemsQuery {
            user: Some(user),
            ..Default::default()
        };
        let counts = service
            .get_item_counts_for_name_item(
                BaseItemKind::Person,
                person,
                &[BaseItemKind::Movie, BaseItemKind::Series],
                &filter,
            )
            .await
            .unwrap();
        assert_eq!((counts.movie_count, counts.series_count), (1, 1));
    }

    /// Credits the `Peoples` row `people` on `item`.
    async fn credit(db: &Database, people: Uuid, item: Uuid) {
        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap" ("ItemId","PeopleId","Role","ListOrder","SortOrder")
               VALUES (?1,?2,'Role',0,0)"#,
        )
        .bind(guid_to_db(item))
        .bind(guid_to_db(people))
        .execute(db.writer())
        .await
        .expect("seed people map");
    }

    /// Gives `item` the genre "Shared", the production year 1999 and a credit
    /// for the `Peoples` row `people`, the three things a Genre, a Year and a
    /// Person count by.
    async fn tag_for_every_count(db: &Database, item: Uuid, people: Uuid) {
        seed_item_genre(db, item, "Shared").await;
        set_column(db, item, "ProductionYear", Some("1999".to_owned())).await;
        credit(db, people, item).await;
    }

    /// The by-name rows a Genre "Shared", a Person "Pat Shared" and a Year
    /// 1999 are, as `(kind, id)`.
    async fn seed_name_rows(db: &Database) -> [(BaseItemKind, Uuid); 3] {
        let rows = [
            (BaseItemKind::Genre, Uuid::from_u128(0x7911), "Shared"),
            (BaseItemKind::Person, Uuid::from_u128(0x7912), "Pat Shared"),
            (BaseItemKind::Year, Uuid::from_u128(0x7913), "1999"),
        ];
        for (kind, id, name) in rows {
            seed_named_item(db, id, kind, name).await;
            set_clean_name(db, id, name).await;
        }
        rows.map(|(kind, id, _)| (kind, id))
    }

    /// A by-name item's counts cover only the libraries its viewer may open:
    /// v12.2 `LibraryManager.GetItemCountsForNameItems` runs `AddUserToQuery`,
    /// which sets the `TopParentIds` that `ItemCountService`'s
    /// `ApplyAccessFiltering` then applies. A Genre, a Person and a Year
    /// alike. With no user (an API key) nothing narrows them; a user who may
    /// open no library counts nothing. A by-name `MusicArtist` row carrying
    /// the genre is in no library, and the access filter exempts no by-name
    /// type, so it drops out of the genre's `ArtistCount` for every user, as
    /// it does upstream.
    #[tokio::test]
    async fn name_counts_cover_only_the_viewers_libraries() {
        let db = test_db().await;
        let service = svc(&db);
        let (first, second) = (Uuid::from_u128(0x7901), Uuid::from_u128(0x7902));
        seed_named_item(&db, first, BaseItemKind::CollectionFolder, "First").await;
        seed_named_item(&db, second, BaseItemKind::CollectionFolder, "Second").await;
        let people = Uuid::from_u128(0x790A);
        crate::people_repository::seed_people_row(&db, people, "Pat Shared", "Actor").await;
        for (movie, library) in [(0x7903, first), (0x7904, second)] {
            let movie = Uuid::from_u128(movie);
            crate::test_support::seed_top_parented_item(
                &db,
                movie,
                BaseItemKind::Movie,
                "m",
                library,
            )
            .await;
            tag_for_every_count(&db, movie, people).await;
        }
        let user_id = Uuid::from_u128(0x7905);
        let user = crate::test_support::seed_user_with_defaults(&db, user_id).await;
        sqlx::query(r#"UPDATE "Permissions" SET "Value" = 0 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
            .bind(guid_to_db(user_id))
            .bind(i32::from(
                ferrofin_db::enums::PermissionKind::EnableAllFolders,
            ))
            .execute(db.writer())
            .await
            .expect("revoke");
        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user_id),
            ferrofin_db::enums::PreferenceKind::EnabledFolders,
            &[first.to_string()],
        )
        .await
        .expect("enable the first library only");
        let viewer = InternalItemsQuery {
            user: Some(user),
            ..InternalItemsQuery::default()
        };
        let no_library = InternalItemsQuery {
            user: Some(
                crate::test_support::seed_named_user(&db, Uuid::from_u128(0x7906), "none").await,
            ),
            ..InternalItemsQuery::default()
        };
        let artist = Uuid::from_u128(0x7907);
        seed_named_item(&db, artist, BaseItemKind::MusicArtist, "Band").await;
        seed_item_genre(&db, artist, "Shared").await;
        let related = [BaseItemKind::Movie, BaseItemKind::MusicArtist];
        let counts = |kind, id, filter| {
            let service = &service;
            async move {
                let counts = service
                    .get_item_counts_for_name_item(kind, id, &related, filter)
                    .await
                    .expect("name counts");
                (counts.movie_count, counts.artist_count)
            }
        };

        let anyone = InternalItemsQuery::default();
        for (kind, id) in seed_name_rows(&db).await {
            let artists = i32::from(kind == BaseItemKind::Genre);
            assert_eq!(
                counts(kind, id, &anyone).await,
                (2, artists),
                "{kind:?} with no user"
            );
            assert_eq!(
                counts(kind, id, &viewer).await,
                (1, 0),
                "{kind:?} for a user who may open one library"
            );
            assert_eq!(
                counts(kind, id, &no_library).await,
                (0, 0),
                "{kind:?} for a user who may open no library"
            );
        }
    }

    /// The counts leave out what `ApplyAccessFiltering` hides while
    /// `IncludeOwnedItems` is off, for every caller: an alternate version
    /// whose primary is in the same library, and an owned item that is not an
    /// extra (a local version or a stacked part, which copy their primary's
    /// genres and year). An extra counts, and so does an alternate whose
    /// primary is in another library (`ApplyAlternateVersionFiltering`).
    #[rstest]
    #[case::no_user(false)]
    #[case::user(true)]
    #[tokio::test]
    async fn name_counts_skip_alternate_versions_and_owned_parts(#[case] as_user: bool) {
        let db = test_db().await;
        let service = svc(&db);
        let (films, four_k) = (Uuid::from_u128(0x7921), Uuid::from_u128(0x7922));
        seed_named_item(&db, films, BaseItemKind::CollectionFolder, "Films").await;
        seed_named_item(&db, four_k, BaseItemKind::CollectionFolder, "4K").await;
        let people = Uuid::from_u128(0x792A);
        crate::people_repository::seed_people_row(&db, people, "Pat Shared", "Actor").await;
        let [primary, merged, local, part, other_library] =
            [0x7923, 0x7924, 0x7925, 0x7926, 0x7927].map(Uuid::from_u128);
        for (movie, library) in [
            (primary, films),
            (merged, films),
            (local, films),
            (part, films),
            (other_library, four_k),
        ] {
            crate::test_support::seed_top_parented_item(
                &db,
                movie,
                BaseItemKind::Movie,
                "m",
                library,
            )
            .await;
            tag_for_every_count(&db, movie, people).await;
        }
        crate::test_support::link_alternate_version(&db, merged, primary).await;
        crate::test_support::link_alternate_version(&db, other_library, primary).await;
        // A local version is owned by its primary as well as linked to it
        // (the "Island Night" row of the parity replay); a stacked part is
        // owned only.
        crate::test_support::link_alternate_version(&db, local, primary).await;
        set_column(&db, local, "OwnerId", Some(guid_to_db(primary))).await;
        set_column(&db, part, "OwnerId", Some(guid_to_db(primary))).await;
        let trailer = Uuid::from_u128(0x7928);
        crate::test_support::seed_top_parented_item(
            &db,
            trailer,
            BaseItemKind::Trailer,
            "t",
            films,
        )
        .await;
        tag_for_every_count(&db, trailer, people).await;
        set_column(&db, trailer, "OwnerId", Some(guid_to_db(primary))).await;
        set_column(&db, trailer, "ExtraType", Some("2".to_owned())).await;
        let filter = if as_user {
            InternalItemsQuery {
                user: Some(
                    crate::test_support::seed_user_with_defaults(&db, Uuid::from_u128(0x7929))
                        .await,
                ),
                ..InternalItemsQuery::default()
            }
        } else {
            InternalItemsQuery::default()
        };

        for (kind, id) in seed_name_rows(&db).await {
            let counts = service
                .get_item_counts_for_name_item(
                    kind,
                    id,
                    &[BaseItemKind::Movie, BaseItemKind::Trailer],
                    &filter,
                )
                .await
                .expect("name counts");
            assert_eq!(
                (counts.movie_count, counts.trailer_count),
                (2, 1),
                "{kind:?}: the primary, the other library's alternate and the trailer extra"
            );
        }
    }

    /// v12.2 `ItemCountServiceTests.SeedGenresTaggingSeriesAndEpisodes`: genres
    /// "rolled", "loose" and "empty" (their ids, in that order); a series tagged
    /// "rolled" holding two episodes, the second tagged "rolled" itself; an
    /// untagged series whose episodes carry "loose" and "rolled" on their own;
    /// and a movie tagged "rolled".
    async fn seed_genres_tagging_series_and_episodes(db: &Database) -> Vec<Uuid> {
        let genres: Vec<Uuid> = ["rolled", "loose", "empty"]
            .into_iter()
            .enumerate()
            .map(|(n, _)| Uuid::from_u128(0x4160 + n as u128))
            .collect();
        for (id, name) in genres.iter().zip(["rolled", "loose", "empty"]) {
            seed_named_item(db, *id, BaseItemKind::Genre, name).await;
            set_clean_name(db, *id, name).await;
        }
        let [tagged_series, untagged_series] = [0x4171, 0x4172].map(Uuid::from_u128);
        seed_named_item(db, tagged_series, BaseItemKind::Series, "Tagged").await;
        seed_named_item(db, untagged_series, BaseItemKind::Series, "Untagged").await;
        let [of_tagged, tagged_of_tagged, loose, rolled_loose] =
            [0x4173, 0x4174, 0x4175, 0x4176].map(Uuid::from_u128);
        for (episode, series) in [
            (of_tagged, tagged_series),
            (tagged_of_tagged, tagged_series),
            (loose, untagged_series),
            (rolled_loose, untagged_series),
        ] {
            crate::test_support::seed_item_of_series(
                db,
                episode,
                BaseItemKind::Episode,
                "e",
                series,
            )
            .await;
        }
        let movie = Uuid::from_u128(0x4177);
        seed_named_item(db, movie, BaseItemKind::Movie, "m").await;
        for (item, genre) in [
            (tagged_series, "rolled"),
            (tagged_of_tagged, "rolled"),
            (rolled_loose, "rolled"),
            (loose, "loose"),
            (movie, "rolled"),
        ] {
            seed_item_genre(db, item, genre).await;
        }
        genres
    }

    /// v12.2 `GetItemCountsForNameItems_TaggedSeries_RollsEpisodesUpIntoTheGenre`:
    /// the episodes of a tagged series count towards the genre even untagged,
    /// a tagged episode of a tagged series counts once, and a tagged episode
    /// of an untagged series counts as itself (`CountEpisodesOfTaggedSeriesByCleanName`).
    #[tokio::test]
    async fn a_tagged_series_rolls_its_episodes_up_into_the_genre() {
        let db = test_db().await;
        let genres = seed_genres_tagging_series_and_episodes(&db).await;
        let rows = stored_rows(&db, &genres).await;
        let batch = svc(&db)
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &name_rows(&rows),
                &[
                    BaseItemKind::Episode,
                    BaseItemKind::Series,
                    BaseItemKind::Movie,
                ],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("batch");
        let pair = |id: &Uuid| (batch[id].episode_count, batch[id].series_count);
        assert_eq!(pair(&genres[0]), (3, 1), "rolled");
        assert_eq!(pair(&genres[1]), (1, 0), "loose");
        assert_eq!(pair(&genres[2]), (0, 0), "empty");
        // The total moves with the episodes (`SetEpisodeCount`), as the DTO's
        // ChildCount, the sum of the count fields, does.
        assert_eq!(batch[&genres[0]].item_count, 5, "rolled: 3 + 1 + the movie");
    }

    /// v12.2 `GetItemCountsForNameItems_TaggedSeriesAndEpisodes_MatchesCountingEachNameItemOnItsOwn`:
    /// the page and the single item agree, with the roll-up's related kinds
    /// and without them.
    #[rstest]
    #[case::rolls_up([BaseItemKind::Episode, BaseItemKind::Series, BaseItemKind::Movie])]
    #[case::plain([BaseItemKind::Movie, BaseItemKind::Series, BaseItemKind::MusicAlbum])]
    #[tokio::test]
    async fn the_roll_up_counts_a_page_as_it_counts_each_name(#[case] related: [BaseItemKind; 3]) {
        let db = test_db().await;
        let genres = seed_genres_tagging_series_and_episodes(&db).await;
        let rows = stored_rows(&db, &genres).await;
        let service = svc(&db);
        let filter = InternalItemsQuery::default();
        let batch = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &name_rows(&rows),
                &related,
                &filter,
            )
            .await
            .expect("batch");
        assert_eq!(batch.len(), genres.len());
        // Without Episode among the related kinds nothing rolls up and no
        // episode counts at all: "rolled" is its series and the movie.
        let rolled = &batch[&genres[0]];
        let expected = if related.contains(&BaseItemKind::Episode) {
            (3, 5)
        } else {
            (0, 2)
        };
        assert_eq!((rolled.episode_count, rolled.item_count), expected);
        for id in &genres {
            let single = service
                .get_item_counts_for_name_item(BaseItemKind::Genre, *id, &related, &filter)
                .await
                .expect("single");
            assert_eq!(batch[id], single, "{id}");
        }
    }

    /// v12.2 `GetItemCountsForNameItems_EpisodeAndItsSeriesTaggedDifferently_KeepsTheGenresApart`:
    /// the episode rolls up into its series' genre, and stays a direct count
    /// of its own genre, which no series carries.
    #[tokio::test]
    async fn an_episode_and_its_series_tagged_differently_keep_the_genres_apart() {
        let db = test_db().await;
        let [on_series, on_episode, series, episode] =
            [0x4181, 0x4182, 0x4183, 0x4184].map(Uuid::from_u128);
        for (id, name) in [(on_series, "on-series"), (on_episode, "on-episode")] {
            seed_named_item(&db, id, BaseItemKind::Genre, name).await;
            set_clean_name(&db, id, name).await;
        }
        seed_named_item(&db, series, BaseItemKind::Series, "S").await;
        crate::test_support::seed_item_of_series(&db, episode, BaseItemKind::Episode, "e", series)
            .await;
        seed_item_genre(&db, series, "on-series").await;
        seed_item_genre(&db, episode, "on-episode").await;
        let rows = stored_rows(&db, &[on_series, on_episode]).await;
        let batch = svc(&db)
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &name_rows(&rows),
                &[
                    BaseItemKind::Episode,
                    BaseItemKind::Series,
                    BaseItemKind::Movie,
                ],
                &InternalItemsQuery::default(),
            )
            .await
            .expect("batch");
        assert_eq!(batch[&on_series].episode_count, 1);
        assert_eq!(
            (
                batch[&on_episode].episode_count,
                batch[&on_episode].series_count
            ),
            (1, 0)
        );
    }

    /// Both sides of the roll-up are `ApplyAccessFiltering`'s `related`: an
    /// episode the viewer may not see (a merged alternate here) rolls nothing
    /// up, and nor does an episode the viewer can see whose series sits in a
    /// library the viewer cannot open.
    #[tokio::test]
    async fn the_roll_up_reads_only_what_the_viewer_may_see() {
        let db = test_db().await;
        let [genre, open_series, shut_series] = [0x4191, 0x4192, 0x4193].map(Uuid::from_u128);
        seed_named_item(&db, genre, BaseItemKind::Genre, "Shared").await;
        set_clean_name(&db, genre, "Shared").await;
        let [open, shut] = [0x4194, 0x4195].map(Uuid::from_u128);
        seed_named_item(&db, open, BaseItemKind::CollectionFolder, "Open").await;
        seed_named_item(&db, shut, BaseItemKind::CollectionFolder, "Shut").await;
        let [episode, alternate, hidden] = [0x4196, 0x4197, 0x4198].map(Uuid::from_u128);
        for (series, library) in [(open_series, open), (shut_series, shut)] {
            crate::test_support::seed_top_parented_item(
                &db,
                series,
                BaseItemKind::Series,
                "s",
                library,
            )
            .await;
            seed_item_genre(&db, series, "Shared").await;
        }
        // `hidden` is in the open library, its series in the shut one.
        for (id, series, library) in [
            (episode, open_series, open),
            (alternate, open_series, open),
            (hidden, shut_series, open),
        ] {
            crate::test_support::seed_item_of_series(&db, id, BaseItemKind::Episode, "e", series)
                .await;
            set_column(&db, id, "TopParentId", Some(guid_to_db(library))).await;
        }
        crate::test_support::link_alternate_version(&db, alternate, episode).await;
        let user_id = Uuid::from_u128(0x4199);
        let user = crate::test_support::seed_user_with_defaults(&db, user_id).await;
        sqlx::query(r#"UPDATE "Permissions" SET "Value" = 0 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
            .bind(guid_to_db(user_id))
            .bind(i32::from(
                ferrofin_db::enums::PermissionKind::EnableAllFolders,
            ))
            .execute(db.writer())
            .await
            .expect("revoke");
        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user_id),
            ferrofin_db::enums::PreferenceKind::EnabledFolders,
            &[open.to_string()],
        )
        .await
        .expect("enable the open library only");
        let viewer = InternalItemsQuery {
            user: Some(user),
            ..InternalItemsQuery::default()
        };
        let related = [BaseItemKind::Episode, BaseItemKind::Series];
        let service = svc(&db);
        let pair = |counts: ItemCounts| (counts.episode_count, counts.series_count);
        let anyone = service
            .get_item_counts_for_name_item(
                BaseItemKind::Genre,
                genre,
                &related,
                &InternalItemsQuery::default(),
            )
            .await
            .expect("no user");
        assert_eq!(pair(anyone), (2, 2), "both series, without the alternate");
        let scoped = service
            .get_item_counts_for_name_item(BaseItemKind::Genre, genre, &related, &viewer)
            .await
            .expect("viewer");
        assert_eq!(pair(scoped), (1, 1), "the open library only");
    }

    /// `EXPLAIN QUERY PLAN` for one statement, as the `detail` column of each
    /// step in outer-to-inner order.
    async fn query_plan(db: &Database, sql: &str, binds: usize) -> Vec<String> {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut query = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(explain));
        for _ in 0..binds {
            query = query.bind("x");
        }
        query
            .fetch_all(db.pool())
            .await
            .expect("explain query plan")
            .into_iter()
            .map(|(_, _, _, detail)| detail)
            .collect()
    }

    /// `GetChildCountBatch`'s statements seek their grouped column's own
    /// index, for either access rule and whatever the virtual rule:
    /// `ParentId` for direct children, `SeasonId` for a season's episodes,
    /// and the `LinkedChildren` parent for links. Any other row a statement
    /// needs (a linked child, an alternate's primary) is read by its primary
    /// key.
    ///
    /// The direct count never leaves its covering index
    /// (`FerrofinIX_BaseItems_ChildCount`, migration 0047), unless a user's
    /// parental or tag rules ([`restrict_count_query`]) add columns it does
    /// not hold; the read then visits the row, as it did before the index.
    /// The merged read selects each child's `Id` and `PresentationUniqueKey`,
    /// which the index does not hold either: nothing in the SQL keeps it off
    /// the wider index, only the planner's preference for the narrower
    /// `IX_BaseItems_ParentId`, which this test pins.
    ///
    /// The `+bi."SeasonId" IS NULL` term must not pull the direct count onto
    /// the `SeasonId` index, where most rows are NULL; a user's
    /// alternate-version rule is one primary-key probe per child.
    #[tokio::test]
    async fn the_child_counts_seek_their_grouped_column() {
        let db = test_db().await;
        let by_key = "USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)";
        for has_user in [false, true] {
            for include_virtual in [true, false] {
                for (child_of, count_index, merged_index) in [
                    (
                        ChildOf::Parent,
                        "COVERING INDEX FerrofinIX_BaseItems_ChildCount (ParentId=?)",
                        "INDEX IX_BaseItems_ParentId (ParentId=?)",
                    ),
                    (
                        ChildOf::Season,
                        "INDEX IX_BaseItems_SeasonId (SeasonId=?)",
                        "INDEX IX_BaseItems_SeasonId (SeasonId=?)",
                    ),
                ] {
                    for (sql, index) in [
                        (
                            child_count_sql(child_of, 2, has_user, include_virtual),
                            count_index,
                        ),
                        (
                            merged_children_sql(child_of, 2, has_user, include_virtual),
                            merged_index,
                        ),
                    ] {
                        let plan = query_plan(&db, &sql, 2).await;
                        assert_eq!(plan[0], format!("SEARCH bi USING {index}"), "{sql}");
                        assert!(
                            plan[1..].iter().all(|step| step.contains(by_key)
                                || step.starts_with("CORRELATED")),
                            "{plan:?}"
                        );
                    }
                }
            }
            let plan = query_plan(&db, &linked_count_sql(2, has_user), 2).await;
            assert!(plan[0].starts_with("SEARCH lc USING"), "{plan:?}");
            assert!(plan[0].ends_with("(ParentId=?)"), "{plan:?}");
            assert_eq!(plan[1], format!("SEARCH bi {by_key}"), "{plan:?}");
            assert!(!plan.iter().any(|step| step.contains("SCAN")), "{plan:?}");
        }
    }

    /// A user with parental and tag rules adds them to each child read as
    /// `bi` (the tag subqueries probe by item), and the statements keep
    /// their seeks: no scan of `BaseItems` or `LinkedChildren`.
    #[tokio::test]
    async fn restricted_child_counts_keep_their_seeks() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        let filter = InternalItemsQuery {
            max_parental_rating: Some(ferrofin_model::entities_media::ParentalRatingScore {
                score: 12,
                sub_score: None,
            }),
            exclude_inherited_tags: vec!["horror".to_owned()],
            ..Default::default()
        };
        assert!(crate::translate_query::has_parental_restrictions(&filter));
        for sql in [
            child_count_sql(ChildOf::Parent, 2, true, false),
            child_count_sql(ChildOf::Season, 2, true, false),
            linked_count_sql(2, true),
            merged_children_sql(ChildOf::Season, 2, true, false),
        ] {
            let mut query = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql.clone()));
            for id in ["a", "b"] {
                query = query.bind(id);
            }
            let mut builder = restrict_count_query(query, &filter, None).expect("restrict");
            let mut built = builder.build();
            let args = built.take_arguments().expect("arguments").expect("binds");
            let explain = format!("EXPLAIN QUERY PLAN {}", built.sql().as_str());
            let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(explain), args)
                .fetch_all(db.pool())
                .await
                .expect("explain")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
            assert!(
                plan[0].starts_with("SEARCH bi USING INDEX")
                    || plan[0].starts_with("SEARCH lc USING"),
                "{sql}\n{plan:?}"
            );
            assert!(
                !plan.iter().any(|step| step.starts_with("SCAN bi")
                    || step.starts_with("SCAN lc")
                    || step.contains("SCAN BaseItems")),
                "{sql}\n{plan:?}"
            );
        }
    }

    /// [`query_plan`] for a by-name count statement once
    /// [`access_filtered_count_query`] has added `filter`'s access terms, its
    /// own `binds` placeholders bound first.
    async fn access_filtered_plan(
        db: &Database,
        sql: &str,
        binds: usize,
        filter: &InternalItemsQuery,
    ) -> Vec<String> {
        use sqlx::{Execute, Row};
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()));
        for _ in 0..binds {
            query = query.bind("x");
        }
        let mut builder = access_filtered_count_query(query, filter).expect("access filter");
        let mut built = builder.build();
        let args = built.take_arguments().expect("arguments").expect("binds");
        let explain = format!("EXPLAIN QUERY PLAN {}", built.sql().as_str());
        sqlx::query_with(sqlx::AssertSqlSafe(explain), args)
            .fetch_all(db.pool())
            .await
            .expect("explain")
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect()
    }

    /// The access terms ([`access_filtered_count_query`]: the viewer's
    /// libraries, the parental rules, the alternate-version and owned-item
    /// rules) leave every by-name statement's plan as it was without them.
    /// The person and value aggregates drive from the names asked for and
    /// probe `BaseItems` by primary key; a plain `TopParentId IN (…)` moved
    /// that probe onto `IX_BaseItems_Type_TopParentId_Id`, once per related
    /// type and library (about 25 ms to 470 ms on a 60k-row synthetic
    /// library), which is why the scope is a `+` term. The year aggregate
    /// keeps its `Type` walk.
    #[tokio::test]
    async fn access_filtered_name_counts_keep_their_seeks() {
        let db = test_db().await;
        let scoped = InternalItemsQuery {
            top_parent_ids: vec![Uuid::from_u128(0x7931), Uuid::from_u128(0x7932)],
            max_parental_rating: Some(ferrofin_model::entities_media::ParentalRatingScore {
                score: 12,
                sub_score: None,
            }),
            exclude_inherited_tags: vec!["horror".to_owned()],
            ..Default::default()
        };
        let by_primary_key = "SEARCH bi USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)";
        let statements = [
            ("person", people_name_counts_sql(3, 2), 5),
            (
                "value",
                item_value_counts_sql(3, 2, &[ItemValueType::Genre]),
                5,
            ),
            ("year", production_year_counts_sql(2, 2), 4),
        ];
        for (name, sql, binds) in statements {
            let bare = query_plan(&db, &sql, binds).await;
            for filter in [&InternalItemsQuery::default(), &scoped] {
                let plan = access_filtered_plan(&db, &sql, binds, filter).await;
                assert_eq!(
                    plan[0], bare[0],
                    "{name}: the access terms must not change the driving loop"
                );
                assert!(
                    !plan.iter().any(|s| s.contains("TopParentId=?")),
                    "{name}: the library scope must not be seeked, got: {plan:?}"
                );
                assert!(
                    !plan
                        .iter()
                        .any(|s| s.starts_with("SCAN bi") || s.contains("SCAN BaseItems")),
                    "{name}: no count may scan BaseItems, got: {plan:?}"
                );
                match name {
                    "person" => assert!(
                        plan[0].contains("IX_Peoples_Name")
                            && plan.iter().any(|s| s == by_primary_key),
                        "person: Peoples by Name, then BaseItems by id, got: {plan:?}"
                    ),
                    "value" => assert!(
                        plan[0].starts_with("SEARCH iv ")
                            && plan.iter().any(|s| s == by_primary_key),
                        "value: ItemValues first, then BaseItems by id, got: {plan:?}"
                    ),
                    _ => assert!(
                        plan[0].starts_with("SEARCH bi USING INDEX")
                            && plan[0].ends_with("(Type=?)"),
                        "year: a Type walk, got: {plan:?}"
                    ),
                }
            }
        }
    }

    /// The episode roll-up ([`tagged_series_episodes_query`]) seeds from the
    /// values asked for, reaches each tagged series by primary key, and its
    /// episodes through `IX_BaseItems_SeriesId`, with or without a viewer's
    /// access terms: never a `BaseItems` scan, never a seek on the library
    /// scope, and the per-episode tag test probes `ItemValuesMap` by item.
    #[tokio::test]
    async fn the_tagged_series_roll_up_seeks_from_the_values() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        let scoped = InternalItemsQuery {
            top_parent_ids: vec![Uuid::from_u128(0x41A1), Uuid::from_u128(0x41A2)],
            max_parental_rating: Some(ferrofin_model::entities_media::ParentalRatingScore {
                score: 12,
                sub_score: None,
            }),
            exclude_inherited_tags: vec!["horror".to_owned()],
            ..Default::default()
        };
        for filter in [&InternalItemsQuery::default(), &scoped] {
            let mut builder =
                tagged_series_episodes_query(&["action", "drama"], &[ItemValueType::Genre], filter);
            let mut built = builder.build();
            let args = built.take_arguments().expect("arguments").expect("binds");
            let explain = format!("EXPLAIN QUERY PLAN {}", built.sql().as_str());
            let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(explain), args)
                .fetch_all(db.pool())
                .await
                .expect("explain")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
            let outer = plan.iter().position(|s| s == "SCAN ts").expect("ts drives");
            assert_eq!(
                plan[outer + 1],
                "SEARCH bi USING INDEX IX_BaseItems_SeriesId (SeriesId=?)",
                "the episodes come by their series, got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|s| s.starts_with("SEARCH iv USING INDEX IX_ItemValues_Type_CleanValue")),
                "the tagged series seed from the values, got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|s| s == "SEARCH bi USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"),
                "a tagged series is read by its id, got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|s| s == "SEARCH em USING INDEX IX_ItemValuesMap_ItemId (ItemId=?)"),
                "an episode's own tag is probed by item, got: {plan:?}"
            );
            assert!(
                !plan.iter().any(|s| s.contains("TopParentId=?")
                    || s.starts_with("SCAN bi")
                    || s.contains("SCAN BaseItems")),
                "no library seek and no BaseItems scan, got: {plan:?}"
            );
        }
    }

    /// The page's presentation-key groups: each page id by its primary key,
    /// then its key's folders by `IX_BaseItems_PresentationUniqueKey` — never
    /// a scan of `BaseItems`, never an `IsFolder`- or `TopParentId`-led
    /// composite.
    #[tokio::test]
    async fn the_presentation_key_groups_seek_by_key() {
        let db = test_db().await;
        let plan = query_plan(&db, super::presentation_key_groups_sql(), 1).await;
        assert!(
            !plan
                .iter()
                .any(|s| s.starts_with("SCAN r") || s.starts_with("SCAN m")),
            "no scan of BaseItems, got: {plan:?}"
        );
        assert!(
            plan.iter()
                .any(|s| s.starts_with("SEARCH r ") && s.contains("(Id=?)")),
            "each page id by its primary key, got: {plan:?}"
        );
        assert!(
            plan.iter().any(|s| s.starts_with(
                "SEARCH m USING INDEX IX_BaseItems_PresentationUniqueKey (PresentationUniqueKey=?)"
            )),
            "the members by IX_BaseItems_PresentationUniqueKey, got: {plan:?}"
        );
    }

    /// Both by-name count aggregates must start from the *name* side. Left to
    /// itself SQLite drives them from `BaseItems` by `Type` — a walk of every
    /// item of every counted kind, 12.9 ms per call on a 9.8k-item library —
    /// so the join order is pinned with `CROSS JOIN`. This asserts the pin
    /// still holds against the real migrated schema: the outermost loop is the
    /// name table, and `BaseItems` is reached by id, not by `Type`.
    #[tokio::test]
    async fn by_name_count_aggregates_seek_from_the_name_side() {
        let db = test_db().await;

        let people = people_name_counts_sql(3, 2);
        let plan = query_plan(&db, &people, 5).await;
        let outer = plan.first().expect("a plan step");
        assert!(
            outer.contains("Peoples") && outer.contains("IX_Peoples_Name"),
            "person counts must seek Peoples by Name first, got: {plan:?}"
        );
        assert!(
            plan.iter()
                .any(|s| s.starts_with("SEARCH bi") && s.contains("Id=?")),
            "person counts must reach BaseItems by id, got: {plan:?}"
        );

        let values = item_value_counts_sql(3, 2, &[ItemValueType::Genre]);
        let plan = query_plan(&db, &values, 5).await;
        let outer = plan.first().expect("a plan step");
        assert!(
            outer.starts_with("SCAN iv ") || outer.starts_with("SEARCH iv ") || outer == "SCAN iv",
            "value counts must start at ItemValues, got: {plan:?}"
        );
        assert!(
            plan.iter()
                .any(|s| s.starts_with("SEARCH bi") && s.contains("Id=?")),
            "value counts must reach BaseItems by id, got: {plan:?}"
        );
    }

    /// The folder leaf-count aggregate must be driven by the page's folder ids,
    /// never by `BaseItems."IsFolder"`. Turning the `EXISTS` back into an inner
    /// join lets SQLite pick the `IsFolder` index and walk every non-folder row
    /// in the library per request (23.5 ms vs 1.96 ms on the bench library for
    /// a 24-series page), so the shape is pinned here, branch by branch: the
    /// ancestor branch seeds `AncestorIds` (`anc`) by `ParentItemId`, both
    /// linked branches seed `LinkedChildren` by `ParentId`, the linked-folder
    /// branch reaches its folder's closure (`linked`) by `ParentItemId`, and
    /// `BaseItems` is reached by id. Nothing is scanned but the page's own
    /// `json_each` and the branches' output (`a`).
    ///
    /// The `played` arm carries a second pin: its `UserData` join must be a
    /// `CROSS JOIN`, so the branches stay the OUTERMOST loop and `UserData` is
    /// probed by `ItemId`. As a plain `JOIN`,
    /// `ud."UserId" = ? AND ud."Played" = 1` is itself a driving term and
    /// SQLite seeds from
    /// `FerrofinIX_UserData_UserId_Played_ItemId (UserId=? AND Played=?)` —
    /// every played row the user owns — which cost 9.9 ms on a 16-album page of
    /// the adopted bench database against 0.1 ms pinned.
    #[tokio::test]
    async fn folder_leaf_counts_seek_from_the_page_folders() {
        let db = test_db().await;

        for (label, extra_join, extra_where, binds) in [
            ("total", "", "", 1),
            (
                "played",
                r#" CROSS JOIN "UserData" ud ON ud."ItemId" = a."ItemId""#,
                r#" AND ud."UserId" = ? AND ud."Played" = 1"#,
                2,
            ),
        ] {
            let sql = folder_leaf_count_sql(extra_join, extra_where);
            let plan = query_plan(&db, &sql, binds).await;
            let seeds = |table: &str, column: &str| {
                plan.iter()
                    .filter(|s| s.starts_with(&format!("SEARCH {table} ")) && s.contains(column))
                    .count()
            };
            assert!(
                !plan.iter().any(|s| s.contains("IsFolder=?")),
                "{label} count must not be driven by the IsFolder index, got: {plan:?}"
            );
            assert_eq!(
                (
                    seeds("anc", "(ParentItemId=?)"),
                    seeds("lc", "(ParentId=?)"),
                    seeds("linked", "(ParentItemId=?)"),
                ),
                (1, 2, 1),
                "{label} count must seed every branch by its parent index, got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|s| s.starts_with("SEARCH bi") && s.contains("Id=?")),
                "{label} count must reach BaseItems by id, got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .filter(|s| s.starts_with("SCAN "))
                    .all(|s| s == "SCAN a" || s.starts_with("SCAN json_each ")),
                "{label} count must scan nothing but the page and the branches, got: {plan:?}"
            );
            if label == "played" {
                let branches = plan.iter().position(|s| s == "SCAN a");
                let user_data = plan
                    .iter()
                    .position(|s| s.starts_with("SEARCH ud") && s.contains("ItemId=?"));
                assert!(
                    matches!((branches, user_data), (Some(b), Some(u)) if b < u),
                    "played count must scan the branches first and probe UserData by ItemId, got: {plan:?}"
                );
            }
        }
    }

    /// A folder read by id (no page to batch) counts its leaves through
    /// [`ItemCountService::get_played_and_total_count_from_linked_children`]:
    /// two `build_query` counts over the `DescendantOfId` set with
    /// `IsFolder = 0`. Both must be driven by that descendant set, reaching
    /// `BaseItems` by id. Left to itself SQLite drives them from
    /// `IX_BaseItems_IsFolder_…` (`IsFolder=?`) — every non-folder row in the
    /// library, each probed against the set: ~10 ms per count for one series
    /// on the bench library, ~0.05 ms driven by the set. The linked
    /// BoxSet/Playlist browse shares the shape; unpinned, its row set seeded
    /// from `(TopParentId, IsFolder)`, every non-folder row in the user's
    /// libraries.
    #[tokio::test]
    async fn descendant_leaf_counts_drive_from_the_descendant_set() {
        use sqlx::Row;
        let db = test_db().await;
        let mut filter = InternalItemsQuery::default();
        filter.set_user(seed_user(&db, Uuid::from_u128(0xD0F0)).await);
        let leaves = svc(&db)
            .countable_leaves_query(&filter, Uuid::from_u128(0xD0F1))
            .await
            .unwrap();
        let played = InternalItemsQuery {
            is_played: Some(true),
            ..leaves.clone()
        };
        // The other `DescendantOfId` producer, a recursive linked
        // BoxSet/Playlist browse filtered on `IsFolder`: scoped to the user's
        // libraries (`TopParentId IN …`), sorted and paged. The `TopParentId`
        // indexes must not take over from the descendant set either.
        let browse = InternalItemsQuery {
            is_virtual_item: None,
            top_parent_ids: vec![Uuid::from_u128(0xD0F2), Uuid::from_u128(0xD0F3)],
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::SortName,
                ferrofin_model::dto::SortOrder::Ascending,
            )],
            limit: Some(100),
            ..leaves.clone()
        };
        for (label, query, shape) in [
            ("total", &leaves, QueryShape::Count),
            ("played", &played, QueryShape::Count),
            ("browse", &browse, QueryShape::FullRows),
            ("browse count", &browse, QueryShape::GroupedCount),
        ] {
            let mut builder = build_query(query, shape);
            let mut built = builder.build();
            let args = built.take_arguments().unwrap().unwrap();
            let sql = format!("EXPLAIN QUERY PLAN {}", built.sql().as_str());
            let plan = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .unwrap()
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect::<Vec<_>>();
            assert!(
                !plan.iter().any(|s| s.contains("IsFolder=?")),
                "{label} must not walk the IsFolder index: {plan:#?}"
            );
            // Every `BaseItems` step seeks an id: the outer statement by the
            // primary key, a sorted page's inner row set by
            // `(TopParentId, Id)` per descendant.
            let steps: Vec<&String> = plan
                .iter()
                .filter(|s| s.starts_with("SEARCH bi ") || s.starts_with("SCAN bi"))
                .collect();
            assert!(
                !steps.is_empty()
                    && steps.iter().all(|s| s.starts_with("SEARCH bi ")
                        && (s.ends_with("(Id=?)") || s.ends_with(" AND Id=?)"))),
                "{label} must reach BaseItems by id from the descendant set: {plan:#?}"
            );
            assert_eq!(
                steps[0].as_str(),
                if label == "browse count" {
                    "SEARCH bi USING COVERING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"
                } else {
                    "SEARCH bi USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"
                },
                "{label} must be driven through the primary key: {plan:#?}"
            );
        }
    }

    /// Adds a FURTHER `UserData` row for `(item, user)` under its own
    /// `CustomDataKey`, the way an adopted Jellyfin database carries a row per
    /// version key. [`seed_user_data`] always writes the key `item.to_string()`,
    /// so this is the only way to build the multiplying shape.
    async fn seed_extra_user_data_key(
        db: &Database,
        user: Uuid,
        item: Uuid,
        suffix: usize,
        played: bool,
    ) {
        sqlx::query(
            r#"INSERT INTO "UserData"
               ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
                "PlayCount", "PlaybackPositionTicks", "Played")
               VALUES (?1, ?2, ?3, 0, 0, 0, ?4)"#,
        )
        .bind(guid_to_db(item))
        .bind(guid_to_db(user))
        .bind(format!("{item}-version-{suffix}"))
        .bind(i64::from(played))
        .execute(db.writer())
        .await
        .expect("extra user data key");
    }

    /// Seeds `folders.len()` folders, the nth holding `played` played and
    /// `unplayed` unplayed leaf children, all wired into the `AncestorIds`
    /// closure for one user. Returns the folder ids in order plus the user.
    ///
    /// `extra_keys` adds that many FURTHER `UserData` rows per leaf under
    /// distinct `CustomDataKey`s. That is not a contrivance: `UserData`'s key is
    /// `(ItemId, UserId, CustomDataKey)` and an adopted Jellyfin database really
    /// does carry a row per key (2,154 such `(item, user)` pairs on the bench
    /// database), so the roll-up's `UserData` join multiplies and only the
    /// `COUNT(DISTINCT …)` keeps the answer right.
    async fn seed_played_roll_up(
        db: &Database,
        folders: &[(usize, usize)],
        extra_keys: usize,
    ) -> (Vec<Uuid>, UserEntity) {
        let user_id = Uuid::from_u128(0x5EED_0001);
        let user = seed_user(db, user_id).await;
        let mut parents = Vec::with_capacity(folders.len());
        let mut next = 0u128;
        for (f, &(played, unplayed)) in folders.iter().enumerate() {
            let parent = Uuid::from_u128(0x5EED_1000 + f as u128);
            seed_item(db, parent, BaseItemKind::Folder).await;
            for i in 0..(played + unplayed) {
                next += 1;
                let leaf = Uuid::from_u128(0x5EED_2000 + next);
                seed_item(db, leaf, BaseItemKind::Movie).await;
                sqlx::query(
                    r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#,
                )
                .bind(guid_to_db(leaf))
                .bind(guid_to_db(parent))
                .execute(db.writer())
                .await
                .expect("ancestor");
                seed_user_data(db, user_id, leaf, i < played, None).await;
                for k in 0..extra_keys {
                    seed_extra_user_data_key(db, user_id, leaf, k, i < played).await;
                }
            }
            parents.push(parent);
        }
        (parents, user)
    }

    /// The roll-up's numbers, pinned per folder: the served batch must report
    /// exactly the seeded played/total leaf counts. This is the correctness
    /// oracle the `CROSS JOIN` plan pin must not move.
    #[rstest]
    #[case(&[(2, 1)], 0)]
    #[case(&[(3, 0), (1, 4)], 0)]
    #[case(&[(0, 2), (5, 5), (1, 0)], 0)]
    #[case(&[(2, 1)], 1)]
    #[case(&[(0, 2), (5, 5), (1, 0)], 2)]
    #[tokio::test]
    async fn played_roll_up_counts_every_folder_on_the_page(
        #[case] folders: &[(usize, usize)],
        #[case] extra_keys: usize,
    ) {
        let db = test_db().await;
        let (parents, user) = seed_played_roll_up(&db, folders, extra_keys).await;

        let counts = svc(&db)
            .get_played_and_total_count_batch(&parents, &user)
            .await
            .expect("roll-up");

        for (&parent, &(played, unplayed)) in parents.iter().zip(folders) {
            let got = counts.get(&parent).copied().unwrap_or_default();
            assert_eq!(
                (got.played, got.total),
                (
                    i32::try_from(played).expect("small"),
                    i32::try_from(played + unplayed).expect("small")
                ),
                "folder {parent} played/total"
            );
        }
    }

    /// A leaf is played when ANY of its `UserData` key rows is played — v12.2
    /// `DescendantQueryHelper.IsPlayedBy`'s first arm,
    /// `b.UserData!.Any(u => u.UserId.Equals(userId) && u.Played)`. Both mixed
    /// orders count, and the `DISTINCT` still keeps the leaf from counting
    /// twice.
    #[tokio::test]
    async fn a_leaf_is_played_when_any_of_its_user_data_keys_is() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x5EED_0001);
        let user = seed_user(&db, user_id).await;
        let parent = Uuid::from_u128(0x5EED_1000);
        seed_item(&db, parent, BaseItemKind::Folder).await;

        // Three leaves: base row played + extra unplayed, base unplayed + extra
        // played, and both unplayed. The first two are played, the third is not.
        for (n, (base, extra)) in [(true, false), (false, true), (false, false)]
            .into_iter()
            .enumerate()
        {
            let leaf = Uuid::from_u128(0x5EED_2000 + n as u128);
            seed_item(&db, leaf, BaseItemKind::Movie).await;
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(guid_to_db(leaf))
                .bind(guid_to_db(parent))
                .execute(db.writer())
                .await
                .expect("ancestor");
            seed_user_data(&db, user_id, leaf, base, None).await;
            seed_extra_user_data_key(&db, user_id, leaf, 0, extra).await;
        }

        let counts = svc(&db)
            .get_played_and_total_count_batch(&[parent], &user)
            .await
            .expect("roll-up");
        let got = counts.get(&parent).copied().unwrap_or_default();
        assert_eq!((got.played, got.total), (2, 3));
    }

    /// Appends `child` to `parent`'s `LinkedChildren`, the only place a box set
    /// or playlist keeps its members.
    async fn link_child(db: &Database, parent: Uuid, child: Uuid) {
        sqlx::query(
            r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
               VALUES (?1, (SELECT COALESCE(MAX("SortOrder"), -1) + 1 FROM "LinkedChildren" WHERE "ParentId" = ?1), ?2, 0)"#,
        )
        .bind(guid_to_db(parent))
        .bind(guid_to_db(child))
        .execute(db.writer())
        .await
        .expect("link child");
    }

    /// Box sets and playlists hold their members only as `LinkedChildren`, and
    /// v12.2 `GetPlayedAndTotalCountBatch` (`ItemCountService.cs:597-690`) counts
    /// them through two branches beside the ancestor closure: the linked leaves
    /// (`linkedLeaves`) and the leaves below a linked folder
    /// (`linkedFolderLeaves`, a series in a collection). Each leaf counts once
    /// per folder however many branches or playlist entries reach it.
    #[tokio::test]
    async fn the_batch_counts_linked_leaves_and_the_leaves_of_linked_folders() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x57A0_0001);
        let user = seed_user(&db, user_id).await;
        let boxset = Uuid::from_u128(0x57A0_1001);
        let playlist = Uuid::from_u128(0x57A0_1002);
        let series = Uuid::from_u128(0x57A0_1003);
        let movie = Uuid::from_u128(0x57A0_2001);
        let played_episode = Uuid::from_u128(0x57A0_2002);
        let unplayed_episode = Uuid::from_u128(0x57A0_2003);
        seed_item(&db, boxset, BaseItemKind::BoxSet).await;
        seed_item(&db, playlist, BaseItemKind::Playlist).await;
        crate::test_support::seed_folder_item(&db, series, BaseItemKind::Series, "Show", None)
            .await;
        seed_item(&db, movie, BaseItemKind::Movie).await;
        for episode in [played_episode, unplayed_episode] {
            seed_item(&db, episode, BaseItemKind::Episode).await;
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(guid_to_db(episode))
                .bind(guid_to_db(series))
                .execute(db.writer())
                .await
                .expect("ancestor");
        }
        seed_user_data(&db, user_id, movie, true, None).await;
        seed_user_data(&db, user_id, played_episode, true, None).await;
        seed_user_data(&db, user_id, unplayed_episode, false, None).await;
        link_child(&db, boxset, movie).await;
        link_child(&db, boxset, series).await;
        // Also reached through the linked series: two branches, one leaf.
        link_child(&db, boxset, played_episode).await;
        link_child(&db, playlist, unplayed_episode).await;
        link_child(&db, playlist, movie).await;
        link_child(&db, playlist, unplayed_episode).await;

        let counts = svc(&db)
            .get_played_and_total_count_batch(&[boxset, playlist, series], &user)
            .await
            .expect("roll-up");
        let got = |id: Uuid| {
            let c = counts.get(&id).copied().unwrap_or_default();
            (c.played, c.total)
        };
        assert_eq!(got(boxset), (2, 3), "box set: the movie and both episodes");
        assert_eq!(got(playlist), (1, 2), "playlist: the movie and one episode");
        assert_eq!(got(series), (1, 2), "series: its own closure, unchanged");
    }

    /// The page is one `json_each` bind, so an unpaged browse holding more
    /// folders than SQLite's 32,766-variable limit still counts: three
    /// `IN (?, …)` lists failed past 10,922 folders with "too many SQL
    /// variables".
    #[tokio::test]
    async fn the_batch_takes_a_page_past_the_sqlite_variable_limit() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x57A0_0002);
        let user = seed_user(&db, user_id).await;
        let folders: Vec<Uuid> = (0..33_000_u128)
            .map(|n| Uuid::from_u128(0x57A1_0000 + n))
            .collect();
        // One played leaf under the LAST folder, so the whole page is read.
        let last = folders[folders.len() - 1];
        let leaf = Uuid::from_u128(0x57A2_0001);
        seed_item(&db, last, BaseItemKind::Folder).await;
        seed_item(&db, leaf, BaseItemKind::Movie).await;
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(leaf))
            .bind(guid_to_db(last))
            .execute(db.writer())
            .await
            .expect("ancestor");
        seed_user_data(&db, user_id, leaf, true, None).await;

        let counts = svc(&db)
            .get_played_and_total_count_batch(&folders, &user)
            .await
            .expect("an oversized page counts");
        // Only the folder with a leaf has an entry: the 0/0 rest are left out.
        assert_eq!(counts.len(), 1);
        let got = counts[&last];
        assert_eq!((got.played, got.total), (1, 1));
    }

    /// Belt and braces: the pinned builder must still be the same query. A
    /// join's result set is order-independent, so the two forms cannot disagree
    /// on rows — what this guards is a future edit that changes the shape while
    /// reaching for the plan, including under a multiplying `UserData` join.
    #[rstest]
    #[case(&[(2, 1)], 0)]
    #[case(&[(3, 0), (1, 4)], 0)]
    #[case(&[(0, 2), (5, 5), (1, 0)], 0)]
    #[case(&[(2, 1)], 1)]
    #[case(&[(3, 0), (1, 4)], 3)]
    #[tokio::test]
    async fn the_join_order_pin_does_not_change_the_rolled_up_rows(
        #[case] folders: &[(usize, usize)],
        #[case] extra_keys: usize,
    ) {
        let db = test_db().await;
        let (parents, user) = seed_played_roll_up(&db, folders, extra_keys).await;
        let ids: Vec<String> = parents.iter().copied().map(guid_to_db).collect();
        let page = serde_json::to_string(&ids).expect("page ids");

        let mut rows = Vec::new();
        for join in [
            r#" CROSS JOIN "UserData" ud ON ud."ItemId" = a."ItemId""#,
            r#" JOIN "UserData" ud ON ud."ItemId" = a."ItemId""#,
        ] {
            let sql = folder_leaf_count_sql(join, r#" AND ud."UserId" = ? AND ud."Played" = 1"#);
            let mut got = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql))
                .bind(page.as_str())
                .bind(user.id.as_str())
                .fetch_all(db.pool())
                .await
                .expect("roll-up");
            got.sort();
            rows.push(got);
        }
        assert_eq!(rows[0], rows[1], "the join-order pin changed the counts");
    }

    /// The root ids the whole-server tests inject.
    const ROOTS: RootFolderIds = RootFolderIds {
        user_root: Uuid::from_u128(0xC001),
        aggregate: Uuid::from_u128(0xC002),
    };

    /// The library [`seed_roots_and_library`] seeds under the user root.
    const LIBRARY: Uuid = Uuid::from_u128(0xC003);

    /// Seeds the `AggregateFolder` and the user root ([`ROOTS`]).
    async fn seed_roots(db: &Database) {
        use crate::test_support::seed_folder_item;
        seed_folder_item(
            db,
            ROOTS.aggregate,
            BaseItemKind::AggregateFolder,
            "root",
            None,
        )
        .await;
        seed_folder_item(
            db,
            ROOTS.user_root,
            BaseItemKind::UserRootFolder,
            "MF",
            None,
        )
        .await;
    }

    /// Hangs `folder` directly under the user root, as a library's
    /// `CollectionFolder` is ([`seed_roots`] first).
    async fn under_user_root(db: &Database, folder: Uuid) {
        set_column(db, folder, "ParentId", Some(guid_to_db(ROOTS.user_root))).await;
    }

    /// Seeds the roots and a library under the user root, as a Ferrofin scan
    /// leaves them, and returns the library; [`in_library`] files a leaf there.
    async fn seed_roots_and_library(db: &Database) -> Uuid {
        let library = LIBRARY;
        seed_roots(db).await;
        crate::test_support::seed_folder_item(
            db,
            library,
            BaseItemKind::CollectionFolder,
            "Movies",
            None,
        )
        .await;
        under_user_root(db, library).await;
        library
    }

    /// Writes the `AncestorIds` edge `item` → `ancestor`.
    async fn in_library(db: &Database, item: Uuid, ancestor: Uuid) {
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(item))
            .bind(guid_to_db(ancestor))
            .execute(db.writer())
            .await
            .expect("ancestor");
    }

    /// Seeds the roots and a library ([`seed_roots_and_library`]) and `leaves`
    /// movies in it, the nth played for the returned user when `n < played`,
    /// each carrying `extra_keys` FURTHER `UserData` rows under distinct
    /// `CustomDataKey`s.
    async fn seed_whole_server(
        db: &Database,
        played: usize,
        unplayed: usize,
        extra_keys: usize,
    ) -> (RootFolderIds, UserEntity) {
        let user_id = Uuid::from_u128(0x5EED_0002);
        let user = seed_user(db, user_id).await;
        let library = seed_roots_and_library(db).await;
        for i in 0..(played + unplayed) {
            let leaf = Uuid::from_u128(0xC100 + i as u128);
            seed_item(db, leaf, BaseItemKind::Movie).await;
            in_library(db, leaf, library).await;
            seed_user_data(db, user_id, leaf, i < played, None).await;
            for k in 0..extra_keys {
                seed_extra_user_data_key(db, user_id, leaf, k, i < played).await;
            }
        }
        (ROOTS, user)
    }

    /// The `AggregateFolder` counts the leaves in its libraries, reached as
    /// v12.2 reaches them, through the closure (`ItemCountService.cs:614-654`):
    /// a leaf an older Ferrofin scan filed under a library's
    /// `CollectionFolder` only, one a Jellyfin scan filed with the aggregate
    /// itself in its closure, and one a current scan filed under both (once).
    /// Not an extra, which upstream's aggregate closure never holds; not the
    /// migration's `PLACEHOLDER` row, which holds detached `UserData` (a played
    /// one included); not a by-name row (all stored `IsFolder = 0`); and not a
    /// leaf in no library.
    #[tokio::test]
    async fn the_whole_server_counts_only_the_leaves_in_its_libraries() {
        let db = test_db().await;
        let (roots, user) = seed_whole_server(&db, 1, 1, 0).await;
        let user_id = Uuid::parse_str(&user.id).expect("user id");
        let adopted = Uuid::from_u128(0xC200);
        seed_item(&db, adopted, BaseItemKind::Movie).await;
        in_library(&db, adopted, roots.aggregate).await;
        for (n, kind) in [
            BaseItemKind::Genre,
            BaseItemKind::MusicGenre,
            BaseItemKind::Studio,
            BaseItemKind::Person,
            BaseItemKind::Year,
            BaseItemKind::MusicArtist,
        ]
        .into_iter()
        .enumerate()
        {
            seed_named_item(&db, Uuid::from_u128(0xC210 + n as u128), kind, "by name").await;
        }
        let loose = Uuid::from_u128(0xC220);
        seed_item(&db, loose, BaseItemKind::Movie).await;
        seed_user_data(&db, user_id, loose, true, None).await;
        let placeholder =
            Uuid::parse_str(crate::translate_query::PLACEHOLDER_ID).expect("placeholder id");
        seed_user_data(&db, user_id, placeholder, true, None).await;

        // A current scan's leaf, its closure holding the library, the
        // location folder and the aggregate: one leaf, however many roots
        // reach it.
        let location = Uuid::from_u128(0xC230);
        crate::test_support::seed_folder_item(&db, location, BaseItemKind::Folder, "loc", None)
            .await;
        set_column(&db, location, "ParentId", Some(guid_to_db(roots.aggregate))).await;
        let current = Uuid::from_u128(0xC231);
        seed_item(&db, current, BaseItemKind::Movie).await;
        for ancestor in [LIBRARY, location, roots.aggregate] {
            in_library(&db, current, ancestor).await;
        }
        seed_user_data(&db, user_id, current, true, None).await;
        // An extra, owned and filed with its library only: upstream's
        // aggregate closure never holds it.
        let extra = Uuid::from_u128(0xC232);
        seed_item(&db, extra, BaseItemKind::Trailer).await;
        set_column(&db, extra, "OwnerId", Some(guid_to_db(current))).await;
        set_column(&db, extra, "ExtraType", Some("2".to_owned())).await;
        in_library(&db, extra, LIBRARY).await;
        seed_user_data(&db, user_id, extra, true, None).await;

        let got = svc(&db)
            .with_root_ids(roots)
            .get_played_and_total_count_batch(&[roots.aggregate], &user)
            .await
            .expect("whole-server counts")[&roots.aggregate];
        assert_eq!(
            (got.played, got.total),
            (2, 4),
            "the two library leaves, the adopted one and the current one; two played"
        );
    }

    /// The `AggregateFolder`'s played/total pair, pinned against duplicate
    /// `UserData` key rows.
    ///
    /// v12.2 `ItemCountService.GetPlayedAndTotalCountFromQuery` (`:735-749`)
    /// projects one boolean PER ITEM — `IsPlayedBy`, whose first arm is
    /// `b.UserData!.Any(u => u.UserId.Equals(userId) && u.Played)` — so a leaf with
    /// four played key rows is ONE played leaf. A bare `COUNT(*)` over the
    /// `UserData` join counted it four times: 4,031 played leaves against a true
    /// 1,574 on the adopted bench database, out of 23,928. `extra_keys > 0` is
    /// that exact shape.
    #[rstest]
    #[case(2, 1, 0)]
    #[case(0, 3, 0)]
    #[case(3, 0, 0)]
    #[case(2, 1, 1)]
    #[case(2, 1, 3)]
    #[case(5, 5, 2)]
    #[tokio::test]
    async fn the_whole_server_counts_each_leaf_once_however_many_user_data_keys_it_has(
        #[case] played: usize,
        #[case] unplayed: usize,
        #[case] extra_keys: usize,
    ) {
        let db = test_db().await;
        let (roots, user) = seed_whole_server(&db, played, unplayed, extra_keys).await;

        let counts = svc(&db)
            .with_root_ids(roots)
            .get_played_and_total_count_batch(&[roots.aggregate], &user)
            .await
            .expect("whole-server counts");

        let got = counts.get(&roots.aggregate).copied().unwrap_or_default();
        assert_eq!(
            (got.played, got.total),
            (
                i32::try_from(played).expect("small"),
                i32::try_from(played + unplayed).expect("small")
            ),
            "aggregate played/total with {extra_keys} extra UserData key rows per leaf"
        );
        // Read by id, the aggregate counts the same whole server.
        let mut filter = InternalItemsQuery::default();
        filter.set_user(user);
        let by_id = svc(&db)
            .with_root_ids(roots)
            .get_played_and_total_count(&filter, roots.aggregate)
            .await
            .expect("per-item whole-server counts");
        assert_eq!((by_id.played, by_id.total), (got.played, got.total));
    }

    /// A whole-server leaf is played when ANY of its `UserData` key rows is —
    /// the same `UserData.Any(…)` rule [`a_leaf_is_played_when_any_of_its_user_data_keys_is`]
    /// pins for the closure arm, asserted here for the `AggregateFolder` arm,
    /// which is a different statement and so cannot inherit it.
    ///
    /// One leaf carries TWO played key rows, so this also fails against the old
    /// `COUNT(*)` (which read 4 played leaves out of 4). Without that leaf it
    /// would pin only the `Any` rule and pass under either form.
    #[tokio::test]
    async fn a_whole_server_leaf_is_played_when_any_of_its_user_data_keys_is() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x5EED_0002);
        let user = seed_user(&db, user_id).await;
        let library = seed_roots_and_library(&db).await;

        // Base row played + extra unplayed, base unplayed + extra played, BOTH
        // played, and both unplayed. The first three are played leaves, the
        // fourth is not.
        for (n, (base, extra)) in [(true, false), (false, true), (true, true), (false, false)]
            .into_iter()
            .enumerate()
        {
            let leaf = Uuid::from_u128(0xC100 + n as u128);
            seed_item(&db, leaf, BaseItemKind::Movie).await;
            in_library(&db, leaf, library).await;
            seed_user_data(&db, user_id, leaf, base, None).await;
            seed_extra_user_data_key(&db, user_id, leaf, 0, extra).await;
        }

        let counts = svc(&db)
            .with_root_ids(ROOTS)
            .get_played_and_total_count_batch(&[ROOTS.aggregate], &user)
            .await
            .expect("whole-server counts");
        let got = counts.get(&ROOTS.aggregate).copied().unwrap_or_default();
        assert_eq!((got.played, got.total), (3, 4));
    }

    /// Another user's played rows are not this user's played leaves — the
    /// `UserId` bind is what makes the whole-server arm per-user, and a
    /// `DISTINCT` over the wrong scope would hide its loss.
    #[tokio::test]
    async fn the_whole_server_played_arm_is_scoped_to_one_user() {
        let db = test_db().await;
        let (roots, _seeded_user) = seed_whole_server(&db, 2, 1, 2).await;
        let other =
            crate::test_support::seed_named_user(&db, Uuid::from_u128(0x5EED_0003), "other").await;

        let counts = svc(&db)
            .with_root_ids(roots)
            .get_played_and_total_count_batch(&[roots.aggregate], &other)
            .await
            .expect("whole-server counts");

        let got = counts.get(&roots.aggregate).copied().unwrap_or_default();
        assert_eq!(
            (got.played, got.total),
            (0, 3),
            "the other user has played nothing, but the total is the server's"
        );
    }

    /// The whole-server played arm must keep driving from the user's played
    /// `UserData` rows, never from `BaseItems."IsFolder"`.
    ///
    /// This is the mirror of [`folder_leaf_counts_seek_from_the_page_folders`]
    /// and the reason the count is a `DISTINCT` over a join rather than an
    /// `EXISTS`: with no closure driving it, an `EXISTS` leaf test leaves
    /// `IsFolder = 0` as the only driving term and SQLite walks every leaf row
    /// on the server, probing `UserData` per row — 15.9 ms against 1.6 on the
    /// adopted bench database. Driven from
    /// `FerrofinIX_UserData_UserId_Played_ItemId (UserId=? AND Played=?)` it
    /// touches only the rows this user played.
    #[tokio::test]
    async fn whole_server_leaf_counts_seek_from_the_user() {
        let db = test_db().await;
        let plan = query_plan(&db, WHOLE_SERVER_PLAYED_COUNT_SQL, 3).await;
        assert!(
            plan.iter().any(
                |s| s.starts_with("SEARCH anc ") && s.contains("(ItemId=? AND ParentItemId=?)")
            ),
            "a played leaf's roots are a primary-key probe, got: {plan:?}"
        );

        assert!(
            !plan.iter().any(|s| s.contains("IsFolder=?")),
            "the whole-server played arm must not be driven by the IsFolder index, got: {plan:?}"
        );
        let Some(first) = plan
            .iter()
            .find(|s| s.starts_with("SEARCH") || s.starts_with("SCAN"))
        else {
            panic!("the whole-server played arm produced no driving loop: {plan:?}");
        };
        assert!(
            first.contains(" ud ") && first.contains("UserId=?"),
            "the whole-server played arm must be driven by UserData (UserId=?), got: {plan:?}"
        );
        assert!(
            plan.iter()
                .any(|s| s.starts_with("SEARCH bi") && s.contains("Id=?")),
            "the whole-server played arm must reach BaseItems by id, got: {plan:?}"
        );
    }

    /// The whole-server total arm seeds `AncestorIds` from the roots
    /// (`ParentItemId=?`) and tests each row as a leaf by primary key: never
    /// driven by `BaseItems."IsFolder"`, never a `BaseItems` scan, the roots
    /// themselves found through `IX_BaseItems_ParentId`.
    #[tokio::test]
    async fn whole_server_leaf_counts_seek_from_the_roots() {
        let db = test_db().await;
        let plan = query_plan(&db, WHOLE_SERVER_TOTAL_COUNT_SQL, 2).await;
        assert!(
            plan.iter()
                .any(|s| s.starts_with("SEARCH anc USING") && s.contains("(ParentItemId=?)")),
            "the closure is seeded by root, got: {plan:?}"
        );
        assert!(
            plan.iter().any(|s| s.starts_with("SEARCH bi ")
                && s.ends_with("USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)")),
            "a leaf is tested by its id, got: {plan:?}"
        );
        assert!(
            plan.iter()
                .any(|s| s.starts_with("SEARCH r USING") && s.contains("ParentId=?")),
            "the roots come from IX_BaseItems_ParentId, got: {plan:?}"
        );
        assert!(
            !plan.iter().any(|s| s.contains("IsFolder=?")
                || s.starts_with("SCAN bi")
                || s == "SCAN r"
                || s.starts_with("SCAN r ")
                || s.contains("SCAN BaseItems")),
            "never driven by IsFolder or a BaseItems scan, got: {plan:?}"
        );
    }
}
