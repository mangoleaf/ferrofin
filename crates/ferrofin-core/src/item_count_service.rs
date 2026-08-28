//! [`FerrofinItemCountService`] — the concrete [`ItemCountService`].
//!
//! Port of `ItemCountService`. Counts items matching an [`InternalItemsQuery`]
//! and rolls the per-`Type` counts up into an [`ItemCounts`] the same way C#
//! `GetItemCounts` does. The played/total *descendant* counts in C# recurse the
//! `AncestorIds`/`FerrofinLinkedChildren` closure through the library manager; here the
//! descendant methods use the `AncestorIds` closure table directly for the
//! common hierarchical case, and the deeper linked-folder roll-up is deferred.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::ItemCounts;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemCountService, NameItemRow, PlayedAndTotal};

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::{QueryShape, build_query};

/// The concrete item-count service.
#[derive(Clone)]
pub struct FerrofinItemCountService {
    db: Database,
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
        Self { db }
    }

    /// Counts descendants of `ancestor_id` (via the `AncestorIds` closure) that
    /// also match `filter`, optionally requiring them to be played by the filter's
    /// user. Returns `(played, total)`.
    async fn descendant_counts(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<PlayedAndTotal, ServiceError> {
        // Reuse the translated filter to constrain the matching item set, then
        // intersect with the ancestor's descendant closure.
        let matching = {
            let mut qb = build_query(filter, QueryShape::IdsOnly);
            qb.build_query_scalar::<String>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
        };
        if matching.is_empty() {
            return Ok(PlayedAndTotal::default());
        }

        let descendants: Vec<String> = {
            let mut sql = String::from(
                r#"SELECT a."ItemId" FROM "AncestorIds" a WHERE a."ParentItemId" = ? AND a."ItemId" IN ("#,
            );
            sql.push_str(&placeholders(matching.len()));
            sql.push(')');
            let mut query = sqlx::query_scalar::<_, String>(&sql).bind(guid_to_db(ancestor_id));
            for id in &matching {
                query = query.bind(id.as_str());
            }
            query.fetch_all(self.db.pool()).await.map_err(db_err)?
        };
        if descendants.is_empty() {
            return Ok(PlayedAndTotal::default());
        }

        let total = i32::try_from(descendants.len()).unwrap_or(i32::MAX);
        let Some(user_id) = filter.user_id() else {
            return Ok(PlayedAndTotal { played: 0, total });
        };

        let mut sql = String::from(
            r#"SELECT COUNT(DISTINCT ud."ItemId") FROM "UserData" ud
               WHERE ud."UserId" = ? AND ud."Played" = 1 AND ud."ItemId" IN ("#,
        );
        sql.push_str(&placeholders(descendants.len()));
        sql.push(')');
        let mut query = sqlx::query_scalar::<_, i64>(&sql).bind(guid_to_db(user_id));
        for id in &descendants {
            query = query.bind(id.as_str());
        }
        let played_i64 = query.fetch_one(self.db.pool()).await.map_err(db_err)?;
        Ok(PlayedAndTotal {
            played: i32::try_from(played_i64).unwrap_or(i32::MAX),
            total,
        })
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

            let mut query = sqlx::query_as::<_, (String, String, i64)>(&sql);
            for name in chunk {
                query = query.bind(*name);
            }
            for t in &type_names {
                query = query.bind(*t);
            }
            for (name, type_, count) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
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
}

#[async_trait]
impl ItemCountService for FerrofinItemCountService {
    async fn get_count(&self, filter: &InternalItemsQuery) -> Result<i32, ServiceError> {
        // Same `AddUserToQuery` confinement the item queries get: a user who may
        // see one library must be counted over that library, not the server.
        // Without it `/Items/Counts` handed a restricted account the whole
        // catalogue's totals — numbers it could not then browse to.
        let scoped = crate::item_repository::scope_to_user_libraries(&self.db, filter).await?;
        let filter = scoped.as_ref().unwrap_or(filter);
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
        let scoped = crate::item_repository::scope_to_user_libraries(&self.db, filter).await?;
        let filter = scoped.as_ref().unwrap_or(filter);
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

        // Jellyfin's LibraryController.GetItemCounts never assigns ItemCount, so
        // it serializes as 0 for the top-level endpoint. Match that: build the
        // per-type counts, then zero the grand total.
        let mut counts = counts_from_type_map(&by_type);
        counts.item_count = 0;
        Ok(counts)
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

        // People live in `PeopleBaseItemMap`/`Peoples`, not `ItemValues`, so a Person's
        // filmography is counted by joining the people map on the person's Name — the
        // C# `ItemCountService` Person branch (`m.People.Name == item.Name`). The
        // CleanName/ItemValues path below would count zero for a Person.
        if kind == BaseItemKind::Person {
            return self.people_name_counts(rows, related_item_kinds, out).await;
        }

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
        // ponytail: no access-scoping `bi."Id" IN (...)` clause — Ferrofin implements no per-user
        // parental/library restriction yet, so `access_filter` (user-only) matches every item.
        // The previous code materialized the *entire* accessible id set and bound it as a giant
        // IN — run once PER name-item (N per page), each a full-library scan that filtered
        // nothing. Re-introduce access scoping as a SQL predicate (subquery/join), not an
        // app-materialized id list, when real access restrictions land.
        let _ = access_filter;
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
            let sql = item_value_counts_sql(chunk.len(), type_names.len());

            let mut query = sqlx::query_as::<_, (String, String, i64)>(&sql);
            for clean in chunk {
                query = query.bind(*clean);
            }
            for t in &type_names {
                query = query.bind(*t);
            }
            for (clean, type_, count) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                by_clean
                    .entry(clean)
                    .or_default()
                    .insert(type_, i32::try_from(count).unwrap_or(i32::MAX));
            }
        }

        for (id, clean) in clean_by_id {
            if let Some(by_type) = by_clean.get(clean) {
                out.insert(id, counts_from_type_map(by_type));
            }
        }
        Ok(out)
    }

    async fn get_played_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<i32, ServiceError> {
        Ok(self.descendant_counts(filter, ancestor_id).await?.played)
    }

    async fn get_total_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<i32, ServiceError> {
        Ok(self.descendant_counts(filter, ancestor_id).await?.total)
    }

    async fn get_played_and_total_count(
        &self,
        filter: &InternalItemsQuery,
        ancestor_id: Uuid,
    ) -> Result<PlayedAndTotal, ServiceError> {
        self.descendant_counts(filter, ancestor_id).await
    }

    async fn get_played_and_total_count_from_linked_children(
        &self,
        filter: &InternalItemsQuery,
        parent_id: Uuid,
    ) -> Result<PlayedAndTotal, ServiceError> {
        // Linked-children played/total: count the parent's FerrofinLinkedChildren that
        // match the filter and are played. Only the direct linked children are
        // counted; recursive linked-folder descent is deferred.
        let matching = {
            let mut qb = build_query(filter, QueryShape::IdsOnly);
            qb.build_query_scalar::<String>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
        };
        if matching.is_empty() {
            return Ok(PlayedAndTotal::default());
        }
        let mut sql = String::from(
            r#"SELECT lc."ChildId" FROM "FerrofinLinkedChildren" lc WHERE lc."ParentId" = ? AND lc."ChildId" IN ("#,
        );
        sql.push_str(&placeholders(matching.len()));
        sql.push(')');
        let mut query = sqlx::query_scalar::<_, String>(&sql).bind(guid_to_db(parent_id));
        for id in &matching {
            query = query.bind(id.as_str());
        }
        let children = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        let total = i32::try_from(children.len()).unwrap_or(i32::MAX);
        let Some(user_id) = filter.user_id() else {
            return Ok(PlayedAndTotal { played: 0, total });
        };
        if children.is_empty() {
            return Ok(PlayedAndTotal::default());
        }
        let mut sql = String::from(
            r#"SELECT COUNT(DISTINCT ud."ItemId") FROM "UserData" ud
               WHERE ud."UserId" = ? AND ud."Played" = 1 AND ud."ItemId" IN ("#,
        );
        sql.push_str(&placeholders(children.len()));
        sql.push(')');
        let mut query = sqlx::query_scalar::<_, i64>(&sql).bind(guid_to_db(user_id));
        for id in &children {
            query = query.bind(id.as_str());
        }
        let played = query.fetch_one(self.db.pool()).await.map_err(db_err)?;
        Ok(PlayedAndTotal {
            played: i32::try_from(played).unwrap_or(i32::MAX),
            total,
        })
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
        // Two grouped joins over the `AncestorIds` closure resolve the whole page in
        // one pass each: total leaf descendants per folder, then the played subset.
        // The prior path looped `descendant_counts` per folder, and each call
        // re-scanned every leaf in the library (a folder-independent id set) before
        // intersecting via a thousand-parameter `IN` — O(folders × library) with no
        // batching, which dominated the by-name browse endpoints.
        let ids: Vec<String> = folder_ids.iter().copied().map(guid_to_db).collect();
        let grouped = |extra_join: &str, extra_where: &str| {
            folder_leaf_count_sql(ids.len(), extra_join, extra_where)
        };

        let total_sql = grouped("", "");
        let mut total_q = sqlx::query_as::<_, (String, i64)>(&total_sql);
        for id in &ids {
            total_q = total_q.bind(id.as_str());
        }
        let totals = total_q.fetch_all(self.db.pool()).await.map_err(db_err)?;

        // Played subset: the same closure, joined to this user's played `UserData`.
        let played_sql = grouped(
            r#" JOIN "UserData" ud ON ud."ItemId" = a."ItemId""#,
            r#" AND ud."UserId" = ? AND ud."Played" = 1"#,
        );
        // The `?` for UserId precedes the `ParentItemId` in-list, so bind it first.
        let mut played_q = sqlx::query_as::<_, (String, i64)>(&played_sql).bind(user.id.as_str());
        for id in &ids {
            played_q = played_q.bind(id.as_str());
        }
        let played_rows = played_q.fetch_all(self.db.pool()).await.map_err(db_err)?;
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
        // Folders with no leaf descendants (e.g. by-name items, which have no
        // `AncestorIds` closure) are absent from the grouped rows ⇒ default 0/0,
        // exactly as the prior per-folder path returned.
        for (&folder, key) in folder_ids.iter().zip(&ids) {
            out.insert(folder, by_parent.get(key).copied().unwrap_or_default());
        }
        Ok(out)
    }

    async fn get_child_count_batch(
        &self,
        parent_ids: &[Uuid],
        _user_id: Option<Uuid>,
    ) -> Result<HashMap<Uuid, i32>, ServiceError> {
        if parent_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // C# `ItemCountService.GetChildCountBatch`: one grouped count of direct
        // `BaseItems` children plus one of `FerrofinLinkedChildren` rows; a parent with
        // linked children reports those instead of its hierarchical children.
        //
        // A Jellyfin library is virtual, so its children hang off its physical
        // folders and grouping on the raw `ParentId` would report 0 for every
        // library on an adopted database — the same translation the item
        // repository does for a browse (`physical_folders_by_view`). Empty, and
        // so a no-op, on a Ferrofin-written database.
        let by_view =
            crate::item_repository::physical_folders_by_view(&self.db, parent_ids).await?;
        let counted: Vec<Uuid> = parent_ids
            .iter()
            .flat_map(|p| match by_view.get(p) {
                Some(folders) => folders.clone(),
                None => vec![*p],
            })
            .collect();
        let ids: Vec<String> = counted.iter().copied().map(guid_to_db).collect();
        let grouped_count = |table: &str| {
            let mut sql =
                format!(r#"SELECT "ParentId", COUNT(*) FROM "{table}" WHERE "ParentId" IN ("#);
            sql.push_str(&placeholders(ids.len()));
            sql.push_str(r#") GROUP BY "ParentId""#);
            sql
        };
        let mut hierarchical: HashMap<String, i64> = HashMap::new();
        let mut linked: HashMap<String, i64> = HashMap::new();
        for (table, into) in [
            ("BaseItems", &mut hierarchical),
            ("FerrofinLinkedChildren", &mut linked),
        ] {
            let mut sql = grouped_count(table);
            if table == "BaseItems" {
                // Direct-child counts skip merged alternates too (a season's
                // ChildCount is its DISTINCT episodes, not every version).
                sql = sql.replace(
                    r#"WHERE "ParentId" IN ("#,
                    r#"WHERE "PrimaryVersionId" IS NULL AND "ParentId" IN ("#,
                );
            }
            let mut query = sqlx::query_as::<_, (String, i64)>(&sql);
            for id in &ids {
                query = query.bind(id.as_str());
            }
            into.extend(query.fetch_all(self.db.pool()).await.map_err(db_err)?);
        }

        let mut out = HashMap::with_capacity(parent_ids.len());
        for parent in parent_ids {
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
                    }
                })
                .sum();
            out.insert(*parent, i32::try_from(count).unwrap_or(i32::MAX));
        }
        Ok(out)
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

/// The grouped leaf-descendant count for a page of folders: `parents` bound
/// folder ids, plus an optional extra join and extra `WHERE` fragment (the
/// played subset adds a `UserData` join).
///
/// The leaf test is an `EXISTS`, not a `JOIN`, and that is load-bearing. As an
/// inner join, `bi."IsFolder" = 0` is a predicate SQLite may use to DRIVE the
/// query: with more than a handful of parents it picks
/// `IX_BaseItems_IsFolder_TopParentId_IsVirtualItem_PresentationUniqueKey_DateCreated
/// (IsFolder=?)` and walks EVERY non-folder row in the library (people
/// included), probing `AncestorIds` per row and sorting the result into a temp
/// b-tree for the `GROUP BY` — O(library), not O(descendants). An `EXISTS`
/// cannot be a driving term, so the plan stays `AncestorIds (ParentItemId=?)`
/// → `BaseItems (Id=?)`. `bi."Id"` is unique, so the inner join could never
/// multiply rows: the two forms are exactly equivalent.
///
/// Measured on the bench library (9,862 items / 6,795 ancestor edges) for the
/// 24-series page `/Items?includeItemTypes=Series`, alternating legs in
/// process: 23.5 ms → 1.96 ms p50, rows identical.
///
/// Merged alternate versions (`PrimaryVersionId` set) are hidden duplicates of
/// their primary — counting them inflated every series/season episode total
/// after a merge-versions pass.
fn folder_leaf_count_sql(parents: usize, extra_join: &str, extra_where: &str) -> String {
    let mut sql = format!(
        r#"SELECT a."ParentItemId", COUNT(DISTINCT a."ItemId")
           FROM "AncestorIds" a{extra_join}
           WHERE EXISTS (SELECT 1 FROM "BaseItems" bi
                         WHERE bi."Id" = a."ItemId"
                           AND bi."IsFolder" = 0
                           AND bi."PrimaryVersionId" IS NULL){extra_where}
             AND a."ParentItemId" IN ("#
    );
    sql.push_str(&placeholders(parents));
    sql.push_str(r#") GROUP BY a."ParentItemId""#);
    sql
}

/// The per-clean-value count aggregate for the generic by-name kinds
/// (genre/studio/year/artist): `cleans` bound clean values and, when `types` is
/// non-zero, that many stored `Type` names to restrict to.
///
/// Same join-order pin as [`people_name_counts_sql`], for the same reason:
/// driven from `BaseItems` the planner walks every item of every counted type
/// before the clean-value filter can reject anything, where seeding from
/// `ItemValues` — one row per genre/studio/year name — seeks straight to the
/// values asked for.
fn item_value_counts_sql(cleans: usize, types: usize) -> String {
    let mut sql = format!(
        r#"SELECT iv."CleanValue", bi."Type", COUNT(DISTINCT bi."Id")
           FROM "ItemValues" iv
           CROSS JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
           CROSS JOIN "BaseItems" bi ON bi."Id" = ivm."ItemId"
           WHERE iv."CleanValue" IN ({})"#,
        placeholders(cleans)
    );
    if types > 0 {
        sql.push_str(r#" AND bi."Type" IN ("#);
        sql.push_str(&placeholders(types));
        sql.push(')');
    }
    sql.push_str(r#" GROUP BY iv."CleanValue", bi."Type""#);
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
    use super::*;
    use crate::test_support::{
        fetch_item, seed_child_item, seed_item, seed_item_genre, seed_item_with_data,
        seed_named_item, seed_user, seed_user_data, set_clean_name, test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_db::entities::base_items::BaseItemEntity;

    fn svc(db: &Database) -> FerrofinItemCountService {
        FerrofinItemCountService::new(db.clone())
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

    /// A Jellyfin library's `ChildCount` is its physical folders' children.
    ///
    /// Nothing carries a `CollectionFolder`'s id as `ParentId` on an adopted
    /// database, so grouping on the raw column reports 0 while the very same
    /// browse returns the items — the two would disagree in one response.
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
        assert_eq!(counts[&view], 2, "the view counts through its folder");
        assert_eq!(counts[&plain], 1, "an ordinary parent counts its own");
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
        // closure-less folder defaults to 0/0.
        let batch = service
            .get_played_and_total_count_batch(&[folder, parent], &user_entity)
            .await
            .expect("batch");
        assert_eq!(batch[&folder].total, 2);
        assert_eq!(batch[&folder].played, 1);
        assert_eq!(batch[&parent].total, 0);
        assert_eq!(batch[&parent].played, 0);

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
                    r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType")
                       VALUES (?1, ?2, 0)"#,
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
                r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType")
                   VALUES (?1, ?2, 0)"#,
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
    async fn get_item_counts_zeroes_the_grand_total() {
        let db = test_db().await;
        let service = svc(&db);

        // Two movies and one series → per-type counts populate, but the
        // top-level ItemCount must serialize as 0 (Jellyfin never assigns it).
        seed_item(&db, Uuid::from_u128(0xCC01), BaseItemKind::Movie).await;
        seed_item(&db, Uuid::from_u128(0xCC02), BaseItemKind::Movie).await;
        seed_item(&db, Uuid::from_u128(0xCC03), BaseItemKind::Series).await;

        let counts = service
            .get_item_counts(&InternalItemsQuery::default())
            .await
            .expect("item counts");

        assert_eq!(counts.movie_count, 2);
        assert_eq!(counts.series_count, 1);
        assert_eq!(counts.item_count, 0);
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
    }

    /// `EXPLAIN QUERY PLAN` for one statement, as the `detail` column of each
    /// step in outer-to-inner order.
    async fn query_plan(db: &Database, sql: &str, binds: usize) -> Vec<String> {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut query = sqlx::query_as::<_, (i64, i64, i64, String)>(&explain);
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

        let values = item_value_counts_sql(3, 2);
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

    /// The folder leaf-count aggregate must be driven by the ancestor closure,
    /// never by `BaseItems."IsFolder"`. Turning the `EXISTS` back into an inner
    /// join lets SQLite pick the `IsFolder` index and walk every non-folder row
    /// in the library per request (23.5 ms vs 1.96 ms on the bench library for
    /// a 24-series page), so the shape is pinned here: `AncestorIds` is the
    /// outermost loop and `BaseItems` is reached by id.
    ///
    /// **Only the `total` arm discriminates.** `played` is driven by
    /// `FerrofinIX_UserData_UserId_Played_ItemId (UserId=? AND Played=?)` under
    /// both the `EXISTS` and the inner-join form, so `IsFolder` is never a
    /// candidate driving term there and `bi` is reached by `Id=?` either way —
    /// it passes against the unfixed SQL. It is kept as a smoke check that the
    /// `UserData` variant still builds and still seeks, not as a regression
    /// pin. Anyone re-verifying this test must delete the `total` case, not the
    /// `played` one.
    #[tokio::test]
    async fn folder_leaf_counts_seek_from_the_ancestor_closure() {
        let db = test_db().await;

        for (label, extra_join, extra_where, binds) in [
            ("total", "", "", 24),
            (
                "played",
                r#" JOIN "UserData" ud ON ud."ItemId" = a."ItemId""#,
                r#" AND ud."UserId" = ? AND ud."Played" = 1"#,
                25,
            ),
        ] {
            let sql = folder_leaf_count_sql(24, extra_join, extra_where);
            let plan = query_plan(&db, &sql, binds).await;
            assert!(
                !plan.iter().any(|s| s.contains("IsFolder=?")),
                "{label} count must not be driven by the IsFolder index, got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|s| s.contains(" a ") && s.contains("AncestorIds")),
                "{label} count must reach AncestorIds through one of its indexes, got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|s| s.starts_with("SEARCH bi") && s.contains("Id=?")),
                "{label} count must reach BaseItems by id, got: {plan:?}"
            );
        }
    }
}
