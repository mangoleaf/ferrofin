//! [`SimilarItemsRepository`] — the raw SQL behind [`FerrofinSimilarItemsManager`].
//!
//! Keeps the weighted-overlap scorer, the watch-state user lookup, and the
//! people-name query behind the repository boundary; the manager orchestrates,
//! this queries. No trait: a single in-crate impl with no dependency-injection
//! seam, used only by the similar-items manager.

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::store::guid_to_db;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;

use crate::db_error::db_err;

/// Raw-SQL data access for similar items and movie recommendations.
#[derive(Clone)]
pub(crate) struct SimilarItemsRepository {
    db: Database,
}

impl SimilarItemsRepository {
    /// Creates the repository over the database handle.
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }

    /// Weighted-overlap similar items to `seed_id`, restricted to `seed_type` and
    /// excluding the seed + `exclude_ids`, ranked by score (stable tiebreak).
    ///
    /// Ports `MovieSimilarItemsProvider`'s per-dimension weights: each shared
    /// genre +10, tag +5, studio +5 (`ItemValues` type 2/3/4); each shared
    /// director +50, actor/guest-star +15 (`Peoples.PersonType`). `ItemValues`
    /// rows are deduped per `(Type, CleanValue)`, so a shared value ⇒ a shared
    /// `ItemValueId`, and people are deduped per `(Name, Type)` ⇒ a shared
    /// `PeopleId` across credits — both join on the shared id.
    pub(crate) async fn weighted_similar_items(
        &self,
        seed_id: Uuid,
        seed_type: &str,
        exclude_ids: &[Uuid],
        limit: i32,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let seed = guid_to_db(seed_id);
        let mut sql = String::from(
            r#"SELECT bi.* FROM (
                   SELECT scored."cand" AS id, SUM(scored."w") AS score FROM (
                       SELECT ivm2."ItemId" AS "cand",
                              CASE iv0."Type" WHEN 2 THEN 10 WHEN 3 THEN 5 WHEN 4 THEN 5 ELSE 0 END AS "w"
                       FROM "ItemValuesMap" ivm0
                       JOIN "ItemValues" iv0 ON iv0."ItemValueId" = ivm0."ItemValueId"
                       JOIN "ItemValuesMap" ivm2 ON ivm2."ItemValueId" = ivm0."ItemValueId"
                       WHERE ivm0."ItemId" = ?1 AND iv0."Type" IN (2, 3, 4)
                       UNION ALL
                       SELECT pm2."ItemId" AS "cand",
                              CASE p0."PersonType" WHEN 'Director' THEN 50 ELSE 15 END AS "w"
                       FROM "PeopleBaseItemMap" pm0
                       JOIN "Peoples" p0 ON p0."Id" = pm0."PeopleId"
                       JOIN "PeopleBaseItemMap" pm2 ON pm2."PeopleId" = pm0."PeopleId"
                       WHERE pm0."ItemId" = ?1
                         AND p0."PersonType" IN ('Director', 'Actor', 'GuestStar')
                   ) scored
                   WHERE scored."cand" <> ?1
                   GROUP BY scored."cand"
               ) s
               JOIN "BaseItems" bi ON bi."Id" = s.id
               WHERE bi."Type" = ?2"#,
        );
        // Bind order: ?1 seed, ?2 seed_type, then the excludes, then the limit.
        let mut next = 3;
        if !exclude_ids.is_empty() {
            sql.push_str(r#" AND bi."Id" NOT IN ("#);
            for i in 0..exclude_ids.len() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push('?');
                sql.push_str(&(next + i).to_string());
            }
            sql.push(')');
            next += exclude_ids.len();
        }
        sql.push_str(r#" ORDER BY s.score DESC, bi."SortName" ASC, bi."Id" ASC LIMIT ?"#);
        sql.push_str(&next.to_string());

        let mut query = sqlx::query_as::<_, BaseItemEntity>(&sql)
            .bind(&seed)
            .bind(seed_type);
        for id in exclude_ids {
            query = query.bind(guid_to_db(*id));
        }
        query = query.bind(i64::from(limit.max(0)));
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    /// Loads the full user row for the watch-state (`IsPlayed`/`IsFavorite`)
    /// predicates, which are `EXISTS` sub-selects scoped to the query's user.
    /// One item's provider ids, keyed by provider name.
    ///
    /// A remote similarity provider is keyed by the seed's external id (TMDB,
    /// MusicBrainz), which lives in `BaseItemProviders` rather than on the row.
    pub(crate) async fn provider_ids(
        &self,
        item_id: Uuid,
    ) -> Result<std::collections::HashMap<String, String>, ServiceError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            r#"SELECT "ProviderId", "ProviderValue" FROM "BaseItemProviders"
               WHERE "ItemId" = ?1"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().collect())
    }

    /// The items whose `provider_key` id is one of `values`, as
    /// `(item_id, value)` pairs.
    ///
    /// The targeted form of `ItemRepository::get_items_with_provider_id`: a
    /// remote provider returns tens of ids, and loading every item that has a
    /// TMDB id to intersect them would read the whole library.
    pub(crate) async fn items_with_provider_values(
        &self,
        provider_key: &str,
        values: &[String],
    ) -> Result<Vec<(Uuid, String)>, ServiceError> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for chunk in values.chunks(500) {
            let placeholders = (2..=chunk.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "ItemId", "ProviderValue" FROM "BaseItemProviders"
                   WHERE "ProviderId" = ?1 COLLATE NOCASE
                     AND "ProviderValue" IN ({placeholders}) COLLATE NOCASE"#,
            );
            let mut query = sqlx::query_as::<_, (String, String)>(&sql).bind(provider_key);
            for value in chunk {
                query = query.bind(value);
            }
            for (item_id, value) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(id) = Uuid::parse_str(&item_id) {
                    out.push((id, value));
                }
            }
        }
        Ok(out)
    }

    pub(crate) async fn fetch_user(
        &self,
        user_id: Uuid,
    ) -> Result<Option<UserEntity>, ServiceError> {
        sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
            .bind(guid_to_db(user_id))
            .fetch_optional(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// Distinct names of the people of `person_types` credited on any of
    /// `item_ids` (C# `GetPeopleNames`), used to seed the director/actor categories.
    pub(crate) async fn people_names_of(
        &self,
        item_ids: &[Uuid],
        person_types: &[&str],
    ) -> Result<Vec<String>, ServiceError> {
        if item_ids.is_empty() || person_types.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::from(
            r#"SELECT DISTINCT p."Name" FROM "PeopleBaseItemMap" pm
               JOIN "Peoples" p ON p."Id" = pm."PeopleId" WHERE pm."ItemId" IN ("#,
        );
        for i in 0..item_ids.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
        }
        sql.push_str(r#") AND p."PersonType" IN ("#);
        for i in 0..person_types.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
        }
        sql.push(')');

        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for id in item_ids {
            query = query.bind(guid_to_db(*id));
        }
        for t in person_types {
            query = query.bind((*t).to_owned());
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }
}
