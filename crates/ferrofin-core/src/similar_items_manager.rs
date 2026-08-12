//! [`FerrofinSimilarItemsManager`] — the concrete [`SimilarItemsManager`].
//!
//! Port of `Emby.Server.Implementations.Library.SimilarItems` (the object-safe
//! subset). The C# `MovieSimilarItemsProvider` scores every candidate by a
//! **weighted overlap** with the seed and returns the top scorers; that scorer is
//! ported here as a single SQL query over `ItemValuesMap`/`ItemValues` (genres,
//! tags, studios) and `PeopleBaseItemMap`/`Peoples` (directors, actors), summing
//! the C# per-dimension weights per candidate.
//!
//! `get_movie_recommendations` ports `GetMovieRecommendationsAsync`: it builds
//! categories from the user's **watch state** — movies similar to recently-played
//! and to liked/favorited ones, plus the directors and actors of recently-played
//! movies — then round-robins them (recently-played and liked weighted double) and
//! orders by recommendation type. With no user or empty history it returns nothing,
//! matching C# (every category query is user-scoped).
//!
//! Accepted divergences from C#: the provider registry (local + remote providers,
//! caching) is dropped — this is the local scorer only; similar candidates are
//! restricted to the seed's own kind (C# also folds in `Trailer`/`LiveTvProgram`
//! when `EnableExternalContentInSuggestions`); `IsFavoriteOrLiked` is approximated
//! as favorite-only (as elsewhere in the query layer); the person-recommendation
//! IMDb de-dup is dropped; and ties are broken **deterministically** (`SortName`,
//! then `Id`) rather than by C#'s `Random`, so results are stable.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{RecommendationType, SortOrder};
use ferrofin_model::live_tv::ItemSortBy;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{SimilarItemsManager, SimilarItemsRecommendation};
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::persistence::ItemRepository;

use crate::similar_items_repository::SimilarItemsRepository;

/// The default number of similar items returned when the caller gives no limit.
const DEFAULT_SIMILAR_LIMIT: i32 = 10;

/// Recently-played movies sampled to seed the "similar to recently played"
/// categories (C# `GetMovieRecommendationsAsync`: `Limit = 7`).
const RECENTLY_PLAYED_LIMIT: i32 = 7;
/// Liked/favorited movies sampled for the "similar to liked" categories
/// (C#: `Limit = 10`).
const LIKED_LIMIT: i32 = 10;
/// How many of the most-recently-played movies contribute director/actor names
/// (C#: `Take(Math.Min(count, 6))`).
const PEOPLE_SOURCE_LIMIT: usize = 6;

/// The concrete similar-items manager.
#[derive(Clone)]
pub struct FerrofinSimilarItemsManager {
    repo: SimilarItemsRepository,
    items: Arc<dyn ItemRepository>,
}

impl std::fmt::Debug for FerrofinSimilarItemsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinSimilarItemsManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinSimilarItemsManager {
    /// Creates a similar-items manager over the database + injected item repository.
    #[must_use]
    pub fn new(db: Database, items: Arc<dyn ItemRepository>) -> Self {
        Self {
            repo: SimilarItemsRepository::new(db),
            items,
        }
    }

    /// Builds a "similar to `seed`" category, or `None` when the seed has no
    /// similar items (C# skips empty baselines).
    async fn similar_category(
        &self,
        seed: &BaseItemEntity,
        recommendation_type: RecommendationType,
        item_limit: i32,
        dto_options: &DtoOptions,
    ) -> Result<Option<SimilarItemsRecommendation>, ServiceError> {
        let Ok(seed_id) = Uuid::parse_str(&seed.id) else {
            return Ok(None);
        };
        let items = self
            .get_similar_items(seed_id, &[], None, dto_options, Some(item_limit))
            .await?;
        if items.is_empty() {
            return Ok(None);
        }
        Ok(Some(SimilarItemsRecommendation {
            baseline_item_name: seed.name.clone().unwrap_or_default(),
            category_id: seed_id,
            recommendation_type,
            items,
        }))
    }

    /// Builds one category per person `name`: their unplayed movies (C#
    /// `GetPersonRecommendations` — `Person = name`, `IsMovie`, `IsPlayed = false`,
    /// directors additionally filtered to the `Director` credit type). The category
    /// id is `md5(name)`, reproducing C#'s `name.GetMD5()`.
    async fn person_categories(
        &self,
        names: &[String],
        recommendation_type: RecommendationType,
        item_limit: i32,
        user: &UserEntity,
        dto_options: &DtoOptions,
    ) -> Result<Vec<SimilarItemsRecommendation>, ServiceError> {
        let person_types =
            if recommendation_type == RecommendationType::HasDirectorFromRecentlyPlayed {
                vec!["Director".to_owned()]
            } else {
                Vec::new()
            };
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let mut query = InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                recursive: true,
                person: Some(name.clone()),
                person_types: person_types.clone(),
                is_played: Some(false),
                limit: Some(item_limit),
                ..Default::default()
            };
            query.set_user(user.clone());
            let items = self.items.get_item_list(&query).await?;
            let _ = dto_options; // DTO projection happens in the handler, as elsewhere
            if items.is_empty() {
                continue;
            }
            out.push(SimilarItemsRecommendation {
                baseline_item_name: name.clone(),
                category_id: ferrofin_common::extensions::get_md5(name),
                recommendation_type,
                items,
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl SimilarItemsManager for FerrofinSimilarItemsManager {
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
        self.repo
            .weighted_similar_items(
                item_id,
                &seed.type_,
                exclude_ids,
                limit.unwrap_or(DEFAULT_SIMILAR_LIMIT),
            )
            .await
    }

    async fn get_movie_recommendations(
        &self,
        user_id: Option<Uuid>,
        parent_id: Uuid,
        category_limit: i32,
        item_limit: i32,
        dto_options: &DtoOptions,
    ) -> Result<Vec<SimilarItemsRecommendation>, ServiceError> {
        // Recommendations are built from the user's watch state (recently-played +
        // liked movies, and the directors/actors of those). With no user, or an
        // empty history, there is nothing to recommend — matching C#, whose every
        // category query is user-scoped and yields empty without played/liked items.
        let Some(uid) = user_id else {
            return Ok(Vec::new());
        };
        let Some(user) = self.repo.fetch_user(uid).await? else {
            return Ok(Vec::new());
        };
        let cat_limit = usize::try_from(category_limit.max(0)).unwrap_or(0);
        if cat_limit == 0 {
            return Ok(Vec::new());
        }

        // Recently-played movies (C#: IsPlayed, OrderBy DatePlayed desc, Limit 7).
        let mut recent_played_q = InternalItemsQuery {
            parent_id,
            include_item_types: vec![BaseItemKind::Movie],
            recursive: true,
            is_played: Some(true),
            limit: Some(RECENTLY_PLAYED_LIMIT),
            order_by: vec![(ItemSortBy::DatePlayed, SortOrder::Descending)],
            ..Default::default()
        };
        recent_played_q.set_user(user.clone());
        let recently_played = self.items.get_item_list(&recent_played_q).await?;

        // Liked/favorited movies (C#: IsFavoriteOrLiked, Limit 10, minus the above).
        let played_ids: Vec<Uuid> = recently_played
            .iter()
            .filter_map(|m| Uuid::parse_str(&m.id).ok())
            .collect();
        let mut liked_q = InternalItemsQuery {
            parent_id,
            include_item_types: vec![BaseItemKind::Movie],
            recursive: true,
            is_favorite_or_liked: Some(true),
            limit: Some(LIKED_LIMIT),
            exclude_item_ids: played_ids.clone(),
            ..Default::default()
        };
        liked_q.set_user(user.clone());
        let liked = self.items.get_item_list(&liked_q).await?;

        // Directors / actors of the six most-recently-played (C# GetPeopleNames).
        let people_source: Vec<Uuid> = played_ids
            .iter()
            .take(PEOPLE_SOURCE_LIMIT)
            .copied()
            .collect();
        let directors = self
            .repo
            .people_names_of(&people_source, &["Director"])
            .await?;
        let actors = self
            .repo
            .people_names_of(&people_source, &["Actor", "GuestStar"])
            .await?;

        // One category per baseline (empties skipped). Baselines are capped to
        // category_limit — the round-robin can't use more categories than that.
        let mut similar_to_played = Vec::new();
        for seed in recently_played.into_iter().take(cat_limit) {
            if let Some(rec) = self
                .similar_category(
                    &seed,
                    RecommendationType::SimilarToRecentlyPlayed,
                    item_limit,
                    dto_options,
                )
                .await?
            {
                similar_to_played.push(rec);
            }
        }
        let mut similar_to_liked = Vec::new();
        for seed in liked.into_iter().take(cat_limit) {
            if let Some(rec) = self
                .similar_category(
                    &seed,
                    RecommendationType::SimilarToLikedItem,
                    item_limit,
                    dto_options,
                )
                .await?
            {
                similar_to_liked.push(rec);
            }
        }
        let has_director = self
            .person_categories(
                &directors,
                RecommendationType::HasDirectorFromRecentlyPlayed,
                item_limit,
                &user,
                dto_options,
            )
            .await?;
        let has_actor = self
            .person_categories(
                &actors,
                RecommendationType::HasActorFromRecentlyPlayed,
                item_limit,
                &user,
                dto_options,
            )
            .await?;

        Ok(round_robin_categories(
            &[similar_to_played, similar_to_liked, has_director, has_actor],
            cat_limit,
        ))
    }
}

/// Merges the four recommendation streams by round-robin — recently-played and
/// liked are visited twice per pass so they carry double weight (C#'s duplicated
/// enumerators) — up to `cat_limit`, then orders the result by recommendation type.
fn round_robin_categories(
    streams: &[Vec<SimilarItemsRecommendation>; 4],
    cat_limit: usize,
) -> Vec<SimilarItemsRecommendation> {
    let visit_order = [0usize, 0, 1, 1, 2, 3];
    let mut cursors = [0usize; 4];
    let mut out: Vec<SimilarItemsRecommendation> = Vec::with_capacity(cat_limit);
    'fill: loop {
        let mut advanced = false;
        for &stream in &visit_order {
            if out.len() >= cat_limit {
                break 'fill;
            }
            if cursors[stream] < streams[stream].len() {
                out.push(streams[stream][cursors[stream]].clone());
                cursors[stream] += 1;
                advanced = true;
            }
        }
        if !advanced {
            break;
        }
    }
    out.sort_by_key(|c| c.recommendation_type as i32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
    use crate::people_repository::FerrofinPeopleRepository;
    use crate::test_support::{seed_item_genre, seed_user, seed_user_data, test_db};
    use ferrofin_db::Database;
    use ferrofin_db::entities::base_items::PeopleEntity;
    use ferrofin_traits::persistence::{ItemPersistenceService, PeopleRepository};

    fn manager(db: &Database) -> FerrofinSimilarItemsManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinSimilarItemsManager::new(
            db.clone(),
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
        )
    }

    /// Credits `name` (with `person_type`) on `item` through the people
    /// repository — same name ⇒ the same person row across items, so two items
    /// crediting "Chris Director" share a person.
    async fn credit_person(db: &Database, item: Uuid, name: &str, person_type: &str) {
        FerrofinPeopleRepository::new(db.clone())
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
        let movie = ferrofin_db::entities::base_items::BaseItemEntity {
            id: id.to_string(),
            type_: stored_type_name(BaseItemKind::Movie)
                .expect("movie type name")
                .to_owned(),
            name: Some(name.to_owned()),
            genres: Some(genres.to_owned()),
            ..Default::default()
        };
        FerrofinItemPersistenceService::new(db.clone())
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

    #[tokio::test]
    async fn recommendations_are_empty_without_watch_history() {
        // The parity fix: recommendations are built from watch state, so a user
        // who has played/favorited nothing gets no categories (matching Jellyfin,
        // where Ferrofin previously returned DateCreated-recency categories).
        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(0x301)).await;
        seed_movie(&db, Uuid::from_u128(0x302), "Unwatched A", "SciFi").await;
        seed_movie(&db, Uuid::from_u128(0x303), "Unwatched B", "SciFi").await;

        let recs = manager(&db)
            .get_movie_recommendations(
                Uuid::parse_str(&user.id).ok(),
                Uuid::nil(),
                6,
                5,
                &DtoOptions::default(),
            )
            .await
            .expect("recommendations");
        assert!(recs.is_empty(), "no watch history ⇒ no recommendations");
    }

    #[tokio::test]
    async fn recommendations_from_recently_played() {
        // A played movie seeds a "similar to recently played" category holding a
        // genre-sharing candidate.
        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(0x311)).await;
        let user_id = Uuid::parse_str(&user.id).expect("user id");
        let played = Uuid::from_u128(0x312);
        let similar = Uuid::from_u128(0x313);
        seed_movie(&db, played, "Played", "SciFi|Horror").await;
        seed_movie(&db, similar, "Similar", "SciFi").await;
        seed_user_data(&db, user_id, played, true, None).await;

        let recs = manager(&db)
            .get_movie_recommendations(Some(user_id), Uuid::nil(), 6, 5, &DtoOptions::default())
            .await
            .expect("recommendations");

        let played_cat = recs
            .iter()
            .find(|r| r.recommendation_type == RecommendationType::SimilarToRecentlyPlayed)
            .expect("a recently-played category");
        assert_eq!(played_cat.baseline_item_name, "Played");
        let item_names: Vec<_> = played_cat
            .items
            .iter()
            .filter_map(|i| i.name.clone())
            .collect();
        assert!(
            item_names.contains(&"Similar".to_owned()),
            "the genre-sharing movie is recommended; got {item_names:?}"
        );
    }
}
