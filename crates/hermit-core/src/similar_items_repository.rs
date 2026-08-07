//! [`SimilarItemsRepository`] — the raw SQL behind [`HermitSimilarItemsManager`].
//!
//! Keeps the weighted-overlap scorer, the watch-state user lookup, and the
//! people-name query behind the repository boundary; the manager orchestrates,
//! this queries. No trait: a single in-crate impl with no dependency-injection
//! seam, used only by the similar-items manager.

use hermit_db::Database;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use uuid::Uuid;

use hermit_traits::error::ServiceError;

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
        let seed = seed_id.to_string();
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
            query = query.bind(id.to_string());
        }
        query = query.bind(i64::from(limit.max(0)));
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    /// Loads the full user row for the watch-state (`IsPlayed`/`IsFavorite`)
    /// predicates, which are `EXISTS` sub-selects scoped to the query's user.
    pub(crate) async fn fetch_user(
        &self,
        user_id: Uuid,
    ) -> Result<Option<UserEntity>, ServiceError> {
        sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
            .bind(user_id.to_string())
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
            query = query.bind(id.to_string());
        }
        for t in person_types {
            query = query.bind((*t).to_owned());
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }
}
