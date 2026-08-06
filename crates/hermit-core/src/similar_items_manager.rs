//! [`HermitSimilarItemsManager`] — the concrete [`SimilarItemsManager`].
//!
//! Port of `Emby.Server.Implementations.Library.SimilarItems` (the object-safe
//! subset). The C# `MovieSimilarItemsProvider` scores every candidate by a
//! **weighted overlap** with the seed and returns the top scorers; that scorer is
//! ported here as a single SQL query over `ItemValuesMap`/`ItemValues` (genres,
//! tags, studios) and `PeopleBaseItemMap`/`Peoples` (directors, actors), summing
//! the C# per-dimension weights per candidate. "Recommendations" are
//! "because you watched"-style categories seeded from a parent's recent items.
//!
//! Accepted divergences from C#: the provider registry (local + remote providers,
//! caching) is dropped — this is the local scorer only; candidates are restricted
//! to the seed's own kind (C# also folds in `Trailer`/`LiveTvProgram` when
//! `EnableExternalContentInSuggestions`); and ties are broken **deterministically**
//! (`SortName`, then `Id`) rather than by C#'s `Random`, so results are stable.

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::dto::{RecommendationType, SortOrder};
use hermit_model::live_tv::ItemSortBy;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::{SimilarItemsManager, SimilarItemsRecommendation};
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use hermit_traits::persistence::ItemRepository;

use crate::db_error::db_err;

/// The default number of similar items returned when the caller gives no limit.
const DEFAULT_SIMILAR_LIMIT: i32 = 10;

/// The concrete similar-items manager.
#[derive(Clone)]
pub struct HermitSimilarItemsManager {
    db: Database,
    items: Arc<dyn ItemRepository>,
}

impl std::fmt::Debug for HermitSimilarItemsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitSimilarItemsManager")
            .finish_non_exhaustive()
    }
}

impl HermitSimilarItemsManager {
    /// Creates a similar-items manager over the database + injected item repository.
    #[must_use]
    pub fn new(db: Database, items: Arc<dyn ItemRepository>) -> Self {
        Self { db, items }
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
    async fn weighted_similar_items(
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
}

#[async_trait]
impl SimilarItemsManager for HermitSimilarItemsManager {
    async fn get_similar_items(
        &self,
        item_id: Uuid,
        exclude_artist_ids: &[Uuid],
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
        limit: Option<i32>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let Some(seed) = self.items.retrieve_item(item_id).await? else {
            return Ok(Vec::new());
        };
        // Weighted overlap over the seed's kind, excluding the seed itself and any
        // excluded artist ids (C# passes these to skip an artist's own catalog).
        let exclude_ids = exclude_artist_ids;
        self.weighted_similar_items(
            item_id,
            &seed.type_,
            exclude_ids,
            limit.unwrap_or(DEFAULT_SIMILAR_LIMIT),
        )
        .await
    }

    async fn get_movie_recommendations(
        &self,
        _user_id: Option<Uuid>,
        parent_id: Uuid,
        category_limit: i32,
        item_limit: i32,
        dto_options: &DtoOptions,
    ) -> Result<Vec<SimilarItemsRecommendation>, ServiceError> {
        // Seed the categories with the parent's most recent movies; each becomes a
        // "because you watched <movie>" category of items similar to it.
        let recent_query = InternalItemsQuery {
            parent_id,
            include_item_types: vec![BaseItemKind::Movie],
            recursive: true,
            limit: Some(category_limit.max(0)),
            order_by: vec![(ItemSortBy::DateCreated, SortOrder::Descending)],
            ..Default::default()
        };
        let seeds = self.items.get_item_list(&recent_query).await?;

        let mut recommendations = Vec::with_capacity(seeds.len());
        for seed in seeds {
            let Ok(seed_id) = Uuid::parse_str(&seed.id) else {
                continue;
            };
            let similar = self
                .get_similar_items(seed_id, &[], _user_id, dto_options, Some(item_limit))
                .await?;
            if similar.is_empty() {
                continue;
            }
            recommendations.push(SimilarItemsRecommendation {
                baseline_item_name: seed.name.clone().unwrap_or_default(),
                category_id: seed_id,
                recommendation_type: RecommendationType::SimilarToRecentlyPlayed,
                items: similar,
            });
        }
        Ok(recommendations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_persistence_service::HermitItemPersistenceService;
    use crate::item_repository::HermitItemRepository;
    use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
    use crate::people_repository::HermitPeopleRepository;
    use crate::test_support::{seed_item_genre, test_db};
    use hermit_db::Database;
    use hermit_db::entities::base_items::PeopleEntity;
    use hermit_traits::persistence::{ItemPersistenceService, PeopleRepository};

    fn manager(db: &Database) -> HermitSimilarItemsManager {
        let lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        HermitSimilarItemsManager::new(
            db.clone(),
            Arc::new(HermitItemRepository::new(db.clone(), lookup)),
        )
    }

    /// Credits `name` (with `person_type`) on `item` through the people
    /// repository — same name ⇒ the same person row across items, so two items
    /// crediting "Chris Director" share a person.
    async fn credit_person(db: &Database, item: Uuid, name: &str, person_type: &str) {
        HermitPeopleRepository::new(db.clone())
            .update_people(
                item,
                &[PeopleEntity {
                    id: String::new(),
                    name: name.to_owned(),
                    person_type: Some(person_type.to_owned()),
                    ..PeopleEntity::default()
                }],
            )
            .await
            .expect("credit person");
    }

    /// Seeds a movie (pipe-separated `genres` stored on the row) and attaches
    /// each genre through `ItemValues` (the genre filter the similar-items
    /// query applies reads that join).
    async fn seed_movie(db: &Database, id: Uuid, name: &str, genres: &str) {
        let movie = hermit_db::entities::base_items::BaseItemEntity {
            id: id.to_string(),
            type_: stored_type_name(BaseItemKind::Movie)
                .expect("movie type name")
                .to_owned(),
            name: Some(name.to_owned()),
            genres: Some(genres.to_owned()),
            ..Default::default()
        };
        HermitItemPersistenceService::new(db.clone())
            .save_items(&[movie])
            .await
            .expect("seed movie");
        for genre in genres.split('|').filter(|g| !g.is_empty()) {
            seed_item_genre(db, id, genre).await;
        }
    }

    #[tokio::test]
    async fn similar_items_share_a_genre_and_exclude_the_seed() {
        let db = test_db().await;
        // Ids avoid 1 (the query translator's placeholder row id).
        let seed = Uuid::from_u128(0x101);
        seed_movie(&db, seed, "Alien", "SciFi|Horror").await;
        seed_movie(&db, Uuid::from_u128(0x102), "Aliens", "SciFi|Action").await;
        // No genre overlap — excluded.
        seed_movie(&db, Uuid::from_u128(0x103), "Amelie", "Romance").await;
        let mgr = manager(&db);

        let similar = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        let names: Vec<_> = similar.iter().filter_map(|r| r.name.clone()).collect();
        assert!(names.contains(&"Aliens".to_owned()));
        assert!(!names.contains(&"Alien".to_owned()));
        assert!(!names.contains(&"Amelie".to_owned()));
    }

    #[tokio::test]
    async fn weighted_score_ranks_shared_director_over_shared_genre() {
        // Seed shares a director (weight 50) with A and a single genre (weight 10)
        // with B. A must outrank B; C shares nothing and is absent.
        let db = test_db().await;
        let seed = Uuid::from_u128(0x201);
        let a = Uuid::from_u128(0x202);
        let b = Uuid::from_u128(0x203);
        let c = Uuid::from_u128(0x204);
        seed_movie(&db, seed, "Seed", "SciFi").await;
        seed_movie(&db, a, "SharesDirector", "Drama").await; // no genre overlap
        seed_movie(&db, b, "SharesGenre", "SciFi").await;
        seed_movie(&db, c, "SharesNothing", "Romance").await;

        credit_person(&db, seed, "Chris Director", "Director").await;
        credit_person(&db, a, "Chris Director", "Director").await;

        let mgr = manager(&db);
        let similar = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        let names: Vec<_> = similar.iter().filter_map(|r| r.name.clone()).collect();
        assert_eq!(
            names,
            vec!["SharesDirector".to_owned(), "SharesGenre".to_owned()],
            "shared director (50) must outrank shared genre (10); non-sharer absent"
        );
    }

    #[tokio::test]
    async fn missing_seed_yields_no_similar_items() {
        let db = test_db().await;
        let mgr = manager(&db);
        let similar = mgr
            .get_similar_items(Uuid::from_u128(99), &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert!(similar.is_empty());
    }
}
