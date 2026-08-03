//! [`HermitSimilarItemsManager`] — the concrete [`SimilarItemsManager`].
//!
//! Port of `Emby.Server.Implementations.Library` similar-items + movie-suggestion
//! logic (the object-safe subset). The C# path registers per-type similarity
//! providers and scores candidates with a weighted overlap of genres, tags,
//! people, studios, and year proximity. The generic provider registry is dropped
//! (a composition-root concern); at this seam "similar" is a genre-overlap query
//! over the injected [`ItemRepository`], excluding the seed and the given
//! artists, and "recommendations" are "because you watched"-style categories
//! seeded from a parent's recent items.
//!
//! The full weighted scorer (tag/person/studio/year terms) is noted deferred; the
//! genre term it dominates is what this implementation applies.

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::dto::{RecommendationType, SortOrder};
use hermit_model::live_tv::ItemSortBy;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::{SimilarItemsManager, SimilarItemsRecommendation};
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use hermit_traits::persistence::ItemRepository;

/// The default number of similar items returned when the caller gives no limit.
const DEFAULT_SIMILAR_LIMIT: i32 = 10;

/// The concrete similar-items manager.
#[derive(Clone)]
pub struct HermitSimilarItemsManager {
    items: Arc<dyn ItemRepository>,
}

impl std::fmt::Debug for HermitSimilarItemsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitSimilarItemsManager")
            .finish_non_exhaustive()
    }
}

impl HermitSimilarItemsManager {
    /// Creates a similar-items manager over the injected item repository.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>) -> Self {
        Self { items }
    }

    /// The display genres of an item row (its `Genres` column, pipe-split).
    fn genres_of(item: &BaseItemEntity) -> Vec<String> {
        item.genres
            .as_deref()
            .map(|g| {
                g.split('|')
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
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
        let genres = Self::genres_of(&seed);
        // Same kind as the seed, sharing at least one genre, excluding the seed
        // itself and any excluded artist ids (C# passes these to skip an artist's
        // own catalog).
        let mut exclude_ids = vec![item_id];
        exclude_ids.extend_from_slice(exclude_artist_ids);
        let seed_kind = crate::item_type_lookup::kind_from_type_name(&seed.type_)
            .unwrap_or(BaseItemKind::Movie);
        let query = InternalItemsQuery {
            include_item_types: vec![seed_kind],
            genres,
            exclude_item_ids: exclude_ids,
            recursive: true,
            limit: Some(limit.unwrap_or(DEFAULT_SIMILAR_LIMIT)),
            order_by: vec![(ItemSortBy::CommunityRating, SortOrder::Descending)],
            ..Default::default()
        };
        self.items.get_item_list(&query).await
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
    use crate::item_repository::HermitItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::test_support::{seed_item_genre, seed_named_item, test_db};
    use hermit_db::Database;

    fn manager(db: &Database) -> HermitSimilarItemsManager {
        let lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        HermitSimilarItemsManager::new(Arc::new(HermitItemRepository::new(db.clone(), lookup)))
    }

    /// Seeds a movie and attaches each pipe-separated genre through `ItemValues`
    /// (the genre filter the similar-items query applies reads that join).
    async fn seed_movie(db: &Database, id: Uuid, name: &str, genres: &str) {
        seed_named_item(db, id, BaseItemKind::Movie, name).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "Genres" = ?2 WHERE "Id" = ?1"#)
            .bind(id.to_string())
            .bind(genres)
            .execute(db.writer())
            .await
            .expect("set genres");
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
