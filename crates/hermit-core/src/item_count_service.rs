//! [`HermitItemCountService`] — the concrete [`ItemCountService`].
//!
//! Port of `ItemCountService`. Counts items matching an [`InternalItemsQuery`]
//! and rolls the per-`Type` counts up into an [`ItemCounts`] the same way C#
//! `GetItemCounts` does. The played/total *descendant* counts in C# recurse the
//! `AncestorIds`/`LinkedChildren` closure through the library manager; here the
//! descendant methods use the `AncestorIds` closure table directly for the
//! common hierarchical case, and the deeper linked-folder roll-up is deferred.

use std::collections::HashMap;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::users::UserEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::dto::ItemCounts;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::options::InternalItemsQuery;
use hermit_traits::persistence::{ItemCountService, PlayedAndTotal};

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::{QueryShape, build_query};

/// The concrete item-count service.
#[derive(Clone)]
pub struct HermitItemCountService {
    db: Database,
}

impl std::fmt::Debug for HermitItemCountService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitItemCountService")
            .finish_non_exhaustive()
    }
}

impl HermitItemCountService {
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
            let mut query = sqlx::query_scalar::<_, String>(&sql).bind(ancestor_id.to_string());
            for id in &matching {
                query = query.bind(id.clone());
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
        let mut query = sqlx::query_scalar::<_, i64>(&sql).bind(user_id.to_string());
        for id in &descendants {
            query = query.bind(id.clone());
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
        ids: &[Uuid],
        related_item_kinds: &[BaseItemKind],
        mut out: HashMap<Uuid, ItemCounts>,
    ) -> Result<HashMap<Uuid, ItemCounts>, ServiceError> {
        // Resolve each person row's Name.
        let mut name_by_id: Vec<(Uuid, String)> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let sql = format!(
                r#"SELECT "Id","Name" FROM "BaseItems" WHERE "Id" IN ({})"#,
                placeholders(chunk.len())
            );
            let mut query = sqlx::query_as::<_, (String, Option<String>)>(&sql);
            for id in chunk {
                query = query.bind(id.to_string());
            }
            for (row_id, name) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let (Ok(uuid), Some(name)) = (Uuid::parse_str(&row_id), name) {
                    name_by_id.push((uuid, name));
                }
            }
        }
        if name_by_id.is_empty() {
            return Ok(out);
        }

        let type_names: Vec<String> = related_item_kinds
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .map(ToOwned::to_owned)
            .collect();
        let distinct_names: Vec<String> = name_by_id
            .iter()
            .map(|(_, n)| n.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let mut by_name: HashMap<String, HashMap<String, i32>> = HashMap::new();
        for chunk in distinct_names.chunks(500) {
            let mut sql = format!(
                r#"SELECT p."Name", bi."Type", COUNT(DISTINCT bi."Id")
                   FROM "BaseItems" bi
                   JOIN "PeopleBaseItemMap" pm ON pm."ItemId" = bi."Id"
                   JOIN "Peoples" p ON p."Id" = pm."PeopleId"
                   WHERE p."Name" IN ({})"#,
                placeholders(chunk.len())
            );
            if !type_names.is_empty() {
                sql.push_str(r#" AND bi."Type" IN ("#);
                sql.push_str(&placeholders(type_names.len()));
                sql.push(')');
            }
            sql.push_str(r#" GROUP BY p."Name", bi."Type""#);

            let mut query = sqlx::query_as::<_, (String, String, i64)>(&sql);
            for name in chunk {
                query = query.bind(name.clone());
            }
            for t in &type_names {
                query = query.bind(t.clone());
            }
            for (name, type_, count) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                by_name
                    .entry(name)
                    .or_default()
                    .insert(type_, i32::try_from(count).unwrap_or(i32::MAX));
            }
        }

        for (id, name) in name_by_id {
            if let Some(by_type) = by_name.get(&name) {
                out.insert(id, counts_from_type_map(by_type));
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl ItemCountService for HermitItemCountService {
    async fn get_count(&self, filter: &InternalItemsQuery) -> Result<i32, ServiceError> {
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
        // A single by-name item is a batch of one.
        Ok(self
            .get_item_counts_for_name_items(
                kind,
                std::slice::from_ref(&id),
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
        ids: &[Uuid],
        related_item_kinds: &[BaseItemKind],
        access_filter: &InternalItemsQuery,
    ) -> Result<HashMap<Uuid, ItemCounts>, ServiceError> {
        // Every id reports counts (zeros when the row or its CleanName is
        // missing), matching the per-item form's defaults.
        let mut out: HashMap<Uuid, ItemCounts> =
            ids.iter().map(|&id| (id, ItemCounts::default())).collect();
        if ids.is_empty() {
            return Ok(out);
        }

        // People live in `PeopleBaseItemMap`/`Peoples`, not `ItemValues`, so a Person's
        // filmography is counted by joining the people map on the person's Name — the
        // C# `ItemCountService` Person branch (`m.People.Name == item.Name`). The
        // CleanName/ItemValues path below would count zero for a Person.
        if kind == BaseItemKind::Person {
            return self.people_name_counts(ids, related_item_kinds, out).await;
        }

        // Resolve every by-name row's CleanName in one query per chunk.
        let mut clean_by_id: Vec<(Uuid, String)> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let sql = format!(
                r#"SELECT "Id", "CleanName" FROM "BaseItems" WHERE "Id" IN ({})"#,
                placeholders(chunk.len())
            );
            let mut query = sqlx::query_as::<_, (String, Option<String>)>(&sql);
            for id in chunk {
                query = query.bind(id.to_string());
            }
            for (row_id, clean) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let (Ok(uuid), Some(clean)) = (Uuid::parse_str(&row_id), clean) {
                    clean_by_id.push((uuid, clean));
                }
            }
        }
        if clean_by_id.is_empty() {
            return Ok(out);
        }

        let type_names: Vec<String> = related_item_kinds
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .map(ToOwned::to_owned)
            .collect();

        // Count the related items carrying each clean value, grouped by value
        // and type — one query per chunk covers the whole page.
        // ponytail: no access-scoping `bi."Id" IN (...)` clause — Hermit implements no per-user
        // parental/library restriction yet, so `access_filter` (user-only) matches every item.
        // The previous code materialized the *entire* accessible id set and bound it as a giant
        // IN — run once PER name-item (N per page), each a full-library scan that filtered
        // nothing. Re-introduce access scoping as a SQL predicate (subquery/join), not an
        // app-materialized id list, when real access restrictions land.
        let _ = access_filter;
        let distinct_cleans: Vec<String> = clean_by_id
            .iter()
            .map(|(_, c)| c.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let mut by_clean: HashMap<String, HashMap<String, i32>> = HashMap::new();
        for chunk in distinct_cleans.chunks(500) {
            let mut sql = format!(
                r#"SELECT iv."CleanValue", bi."Type", COUNT(DISTINCT bi."Id") FROM "BaseItems" bi
                   JOIN "ItemValuesMap" ivm ON ivm."ItemId" = bi."Id"
                   JOIN "ItemValues" iv ON iv."ItemValueId" = ivm."ItemValueId"
                   WHERE iv."CleanValue" IN ({})"#,
                placeholders(chunk.len())
            );
            if !type_names.is_empty() {
                sql.push_str(r#" AND bi."Type" IN ("#);
                sql.push_str(&placeholders(type_names.len()));
                sql.push(')');
            }
            sql.push_str(r#" GROUP BY iv."CleanValue", bi."Type""#);

            let mut query = sqlx::query_as::<_, (String, String, i64)>(&sql);
            for clean in chunk {
                query = query.bind(clean.clone());
            }
            for t in &type_names {
                query = query.bind(t.clone());
            }
            for (clean, type_, count) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                by_clean
                    .entry(clean)
                    .or_default()
                    .insert(type_, i32::try_from(count).unwrap_or(i32::MAX));
            }
        }

        for (id, clean) in clean_by_id {
            if let Some(by_type) = by_clean.get(&clean) {
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
        // Linked-children played/total: count the parent's LinkedChildren that
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
            r#"SELECT lc."ChildId" FROM "LinkedChildren" lc WHERE lc."ParentId" = ? AND lc."ChildId" IN ("#,
        );
        sql.push_str(&placeholders(matching.len()));
        sql.push(')');
        let mut query = sqlx::query_scalar::<_, String>(&sql).bind(parent_id.to_string());
        for id in &matching {
            query = query.bind(id.clone());
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
        let mut query = sqlx::query_scalar::<_, i64>(&sql).bind(user_id.to_string());
        for id in &children {
            query = query.bind(id.clone());
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
        let ids: Vec<String> = folder_ids.iter().map(Uuid::to_string).collect();
        let grouped = |extra_join: &str, extra_where: &str| {
            let mut sql = format!(
                r#"SELECT a."ParentItemId", COUNT(DISTINCT a."ItemId")
                   FROM "AncestorIds" a
                   JOIN "BaseItems" bi ON bi."Id" = a."ItemId"{extra_join}
                   WHERE bi."IsFolder" = 0{extra_where} AND a."ParentItemId" IN ("#
            );
            sql.push_str(&placeholders(ids.len()));
            sql.push_str(r#") GROUP BY a."ParentItemId""#);
            sql
        };

        let total_sql = grouped("", "");
        let mut total_q = sqlx::query_as::<_, (String, i64)>(&total_sql);
        for id in &ids {
            total_q = total_q.bind(id.clone());
        }
        let totals = total_q.fetch_all(self.db.pool()).await.map_err(db_err)?;

        // Played subset: the same closure, joined to this user's played `UserData`.
        let played_sql = grouped(
            r#" JOIN "UserData" ud ON ud."ItemId" = a."ItemId""#,
            r#" AND ud."UserId" = ? AND ud."Played" = 1"#,
        );
        // The `?` for UserId precedes the `ParentItemId` in-list, so bind it first.
        let mut played_q = sqlx::query_as::<_, (String, i64)>(&played_sql).bind(user.id.clone());
        for id in &ids {
            played_q = played_q.bind(id.clone());
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
        // `BaseItems` children plus one of `LinkedChildren` rows; a parent with
        // linked children reports those instead of its hierarchical children.
        let ids: Vec<String> = parent_ids.iter().map(Uuid::to_string).collect();
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
            ("LinkedChildren", &mut linked),
        ] {
            let sql = grouped_count(table);
            let mut query = sqlx::query_as::<_, (String, i64)>(&sql);
            for id in &ids {
                query = query.bind(id.clone());
            }
            into.extend(query.fetch_all(self.db.pool()).await.map_err(db_err)?);
        }

        let mut out = HashMap::with_capacity(parent_ids.len());
        for (parent, key) in parent_ids.iter().zip(&ids) {
            let linked_count = linked.get(key).copied().unwrap_or(0);
            let count = if linked_count > 0 {
                linked_count
            } else {
                hierarchical.get(key).copied().unwrap_or(0)
            };
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
        seed_item, seed_item_genre, seed_named_item, seed_user, seed_user_data, set_clean_name,
        test_db,
    };
    use hermit_db::Database;

    fn svc(db: &Database) -> HermitItemCountService {
        HermitItemCountService::new(db.clone())
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
                .bind(child.to_string())
                .bind(folder.to_string())
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
            .bind(kid.to_string())
            .bind(parent.to_string())
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
                    r#"INSERT INTO "LinkedChildren" ("ParentId", "ChildId", "ChildType")
                       VALUES (?1, ?2, 0)"#,
                )
                .bind(boxset.to_string())
                .bind(child.to_string())
                .execute(db.writer())
                .await
                .expect("link child");
            } else {
                sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
                    .bind(child.to_string())
                    .bind(boxset.to_string())
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
                r#"INSERT INTO "LinkedChildren" ("ParentId", "ChildId", "ChildType")
                   VALUES (?1, ?2, 0)"#,
            )
            .bind(parent.to_string())
            .bind(child.to_string())
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

        let batch = service
            .get_item_counts_for_name_items(
                BaseItemKind::Genre,
                &[drama, comedy, no_clean],
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
            .bind(people_id.to_string())
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
            .bind(item.to_string())
            .bind(people_id.to_string())
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
}
