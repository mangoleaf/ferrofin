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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{RecommendationType, SortOrder};
use ferrofin_model::live_tv::ItemSortBy;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{
    RemoteSimilarItemsProvider, SimilarItemReference, SimilarItemsManager, SimilarItemsQuery,
    SimilarItemsRecommendation,
};
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::persistence::ItemRepository;

use crate::similar_items_repository::SimilarItemsRepository;

/// Reads an unexpired reference cache, or `None` when it is missing, stale or
/// unparseable.
fn read_reference_cache(path: &Path) -> Option<Vec<SimilarItemReference>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cache: SimilarItemsCache = serde_json::from_str(&raw).ok()?;
    if cache.expires_at <= chrono::Utc::now() {
        return None;
    }
    Some(
        cache
            .references
            .into_iter()
            .map(|r| SimilarItemReference {
                provider_name: r.provider_name,
                provider_id: r.provider_id,
                score: r.score,
            })
            .collect(),
    )
}

/// Writes a reference cache entry, best-effort — a cache that cannot be written
/// only costs a re-fetch next time.
fn write_reference_cache(path: &Path, references: &[SimilarItemReference], ttl: Duration) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(ttl) = chrono::Duration::from_std(ttl) else {
        return;
    };
    let cache = SimilarItemsCache {
        references: references
            .iter()
            .map(|r| CachedReference {
                provider_name: r.provider_name.clone(),
                provider_id: r.provider_id.clone(),
                score: r.score,
            })
            .collect(),
        expires_at: chrono::Utc::now() + ttl,
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = std::fs::write(path, json);
    }
}

/// The similarity score of one result — port of
/// `SimilarItemsManager.CalculateScore`.
///
/// A provider that supplied no score of its own gets one derived from the
/// result's position, and every result gets a small boost for its provider's
/// rank so an earlier provider's hits outrank a later one's at equal position.
fn calculate_score(match_score: Option<f32>, provider_order: usize, position: usize) -> f32 {
    let position = u32::try_from(position).unwrap_or(u32::MAX);
    #[allow(clippy::cast_precision_loss)]
    let base = match_score.unwrap_or(1.0 - (position as f32 * POSITION_SCORE_STEP));
    let rank = PROVIDER_ORDER_HEADROOM.saturating_sub(provider_order);
    #[allow(clippy::cast_precision_loss)]
    let boost = rank as f32 * PROVIDER_ORDER_BOOST;
    (base + boost).clamp(0.0, 1.0)
}

/// How much each position down a provider's result list costs (C# `0.02f`).
const POSITION_SCORE_STEP: f32 = 0.02;
/// The provider ranks that earn a boost at all (C# `Math.Max(0, 10 - order)`).
const PROVIDER_ORDER_HEADROOM: usize = 10;
/// The per-rank boost an earlier provider's results get (C# `0.005f`).
const PROVIDER_ORDER_BOOST: f32 = 0.005;

/// One cached remote result set: the references plus their expiry.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SimilarItemsCache {
    /// The cached references.
    references: Vec<CachedReference>,
    /// When the entry stops being usable.
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// A [`SimilarItemReference`] in the on-disk shape Jellyfin writes.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CachedReference {
    /// The provider-id key.
    provider_name: String,
    /// The provider-id value.
    provider_id: String,
    /// The provider's own score, when it supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f32>,
}

/// The [`BaseItemKind`] of a row, defaulting to `Folder` for an unrecognized
/// stored `Type` (the crate-wide conservative default).
fn kind_of(entity: &BaseItemEntity) -> BaseItemKind {
    crate::item_type_lookup::kind_from_type_name(&entity.type_).unwrap_or(BaseItemKind::Folder)
}

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
    /// The registered remote similarity providers, in registration order. A
    /// provider only runs for a library that ticked it.
    remote: Vec<Arc<dyn RemoteSimilarItemsProvider>>,
    /// Resolves the seed's owning library so its `SimilarItemProviders`
    /// selection and order can be read. Absent → remote providers never run.
    library: Option<Arc<dyn ferrofin_traits::library::VirtualFolderManager>>,
    /// Where a remote provider's references are cached between requests.
    /// Absent → no caching, exactly as a `None` cache duration does.
    cache_dir: Option<PathBuf>,
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
            remote: Vec::new(),
            library: None,
            cache_dir: None,
        }
    }

    /// Registers the remote similarity providers and the library manager whose
    /// per-library `SimilarItemProviders` selection gates them.
    #[must_use]
    pub fn with_remote_providers(
        mut self,
        providers: Vec<Arc<dyn RemoteSimilarItemsProvider>>,
        library: Arc<dyn ferrofin_traits::library::VirtualFolderManager>,
    ) -> Self {
        self.remote = providers;
        self.library = Some(library);
        self
    }

    /// Points the remote-reference cache at `cache_dir` — the same
    /// `{cache}/{provider}-similar-{type}/{itemId}.json` layout Jellyfin uses,
    /// so a shared cache directory stays valid across the two.
    #[must_use]
    pub fn with_cache_dir(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = Some(cache_dir);
        self
    }

    /// The remote providers this library enabled for `kind`, in the admin's
    /// configured order (a provider absent from the order list sorts last).
    ///
    /// Port of the `TypeOptions.SimilarItemProviders` /
    /// `SimilarItemProviderOrder` resolution in `SimilarItemsManager`.
    async fn enabled_remote_providers(
        &self,
        seed: &BaseItemEntity,
        kind: BaseItemKind,
    ) -> Vec<Arc<dyn RemoteSimilarItemsProvider>> {
        let (Some(library), false) = (self.library.as_ref(), self.remote.is_empty()) else {
            return Vec::new();
        };
        let Some(type_name) = seed.type_.rsplit('.').next() else {
            return Vec::new();
        };
        let Ok(folders) = library.get_virtual_folders().await else {
            return Vec::new();
        };
        let options = seed
            .top_parent_id
            .as_deref()
            .and_then(|top| {
                folders
                    .iter()
                    .find(|f| f.item_id.as_deref() == Some(top))
                    .and_then(|f| f.library_options.as_ref())
            })
            .and_then(|o| {
                o.type_options.iter().find(|t| {
                    t.type_
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case(type_name))
                })
            });
        let Some(options) = options else {
            // No saved selection: remote similarity is opt-in, so nothing runs.
            return Vec::new();
        };
        let order = if options.similar_item_provider_order.is_empty() {
            &options.similar_item_providers
        } else {
            &options.similar_item_provider_order
        };
        let mut enabled: Vec<Arc<dyn RemoteSimilarItemsProvider>> = self
            .remote
            .iter()
            .filter(|p| p.supports(kind))
            .filter(|p| {
                options
                    .similar_item_providers
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(p.name()))
            })
            .map(Arc::clone)
            .collect();
        enabled.sort_by_key(|p| {
            order
                .iter()
                .position(|n| n.eq_ignore_ascii_case(p.name()))
                .unwrap_or(usize::MAX)
        });
        enabled
    }

    /// One provider's references for `seed`, read from the disk cache when it
    /// is still fresh and written back after a live fetch.
    ///
    /// Port of `TryReadSimilarItemsCacheAsync`/`SaveSimilarItemsCacheAsync`:
    /// the path and JSON shape match Jellyfin's, so a cache directory shared
    /// with a Jellyfin install stays valid for both.
    async fn remote_references(
        &self,
        provider: &dyn RemoteSimilarItemsProvider,
        seed: &BaseItemEntity,
        seed_provider_ids: &HashMap<String, String>,
        query: &SimilarItemsQuery,
    ) -> Vec<SimilarItemReference> {
        let cache_path = self.cache_path(provider.name(), seed);
        if let Some(path) = cache_path.as_deref()
            && let Some(cached) = read_reference_cache(path)
        {
            return cached;
        }
        let references = provider
            .get_similar_items(seed, seed_provider_ids, query)
            .await;
        if let (Some(path), Some(ttl), false) = (
            cache_path.as_deref(),
            provider.cache_duration(),
            references.is_empty(),
        ) {
            write_reference_cache(path, &references, ttl);
        }
        references
    }

    /// `{cache}/{provider}-similar-{type}/{itemId:N}.json`, lowercased exactly
    /// as C# `GetSimilarItemsCachePath` builds it.
    fn cache_path(&self, provider_name: &str, seed: &BaseItemEntity) -> Option<PathBuf> {
        let root = self.cache_dir.as_ref()?;
        let type_name = seed.type_.rsplit('.').next()?;
        let id = Uuid::parse_str(&seed.id).ok()?;
        Some(
            root.join(format!(
                "{}-similar-{}",
                provider_name.to_lowercase(),
                type_name.to_lowercase()
            ))
            .join(format!("{}.json", id.simple())),
        )
    }

    /// Resolves a provider's references to library items of `kind`, scoring
    /// each and skipping anything already accounted for.
    ///
    /// Port of `SimilarItemsManager.ResolveRemoteReferences`: a reference is
    /// matched by looking its provider id up in `BaseItemProviders`, and the
    /// best (highest-scoring, else earliest) reference per id wins.
    async fn resolve_remote_references(
        &self,
        references: &[SimilarItemReference],
        provider_order: usize,
        kind: BaseItemKind,
        taken: &mut std::collections::HashSet<Uuid>,
    ) -> Vec<(BaseItemEntity, f32)> {
        // Best reference per (provider, id): higher score wins, and at equal
        // score the earlier position does.
        let mut best: HashMap<(String, String), (Option<f32>, usize)> = HashMap::new();
        for (position, reference) in references.iter().enumerate() {
            let key = (
                reference.provider_name.to_lowercase(),
                reference.provider_id.to_lowercase(),
            );
            // C#'s `match.Score > existing.Score` is a *lifted* float comparison:
            // it is false whenever either side is null, so an unscored reference
            // never displaces a scored one and vice versa. `Option`'s own
            // ordering ranks `Some` above `None`, which would diverge — hence
            // the explicit both-present check.
            let better = match best.get(&key) {
                None => true,
                Some((score, at)) => match (reference.score, *score) {
                    (Some(new), Some(old)) if new > old => true,
                    _ => reference.score == *score && position < *at,
                },
            };
            if better {
                best.insert(key, (reference.score, position));
            }
        }

        let mut by_provider: HashMap<&str, Vec<String>> = HashMap::new();
        for reference in references {
            by_provider
                .entry(reference.provider_name.as_str())
                .or_default()
                .push(reference.provider_id.clone());
        }

        let mut out = Vec::new();
        for (provider_key, values) in by_provider {
            let Ok(rows) = self
                .repo
                .items_with_provider_values(provider_key, &values)
                .await
            else {
                continue;
            };
            for (item_id, value) in rows {
                let key = (provider_key.to_lowercase(), value.to_lowercase());
                let Some(&(score, position)) = best.get(&key) else {
                    continue;
                };
                if !taken.insert(item_id) {
                    continue;
                }
                let Ok(Some(entity)) = self.items.retrieve_item(item_id).await else {
                    continue;
                };
                if kind_of(&entity) != kind {
                    continue;
                }
                out.push((entity, calculate_score(score, provider_order, position)));
            }
        }
        out
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
        user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
        limit: Option<i32>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let Some(seed) = self.items.retrieve_item(item_id).await? else {
            return Ok(Vec::new());
        };
        let wanted = limit.unwrap_or(DEFAULT_SIMILAR_LIMIT);
        // The local scorer always runs first (C#: "Local providers are always
        // enabled"), and is provider-order 0.
        let by_overlap = self
            .repo
            .weighted_similar_items(item_id, &seed.type_, exclude_artist_ids, wanted)
            .await?;
        let mut taken: std::collections::HashSet<Uuid> = std::collections::HashSet::from([item_id]);
        let mut scored: Vec<(BaseItemEntity, f32)> = Vec::with_capacity(by_overlap.len());
        for (position, entity) in by_overlap.into_iter().enumerate() {
            if let Ok(id) = Uuid::parse_str(&entity.id)
                && taken.insert(id)
            {
                scored.push((entity, calculate_score(None, 0, position)));
            }
        }

        // Then each remote provider the library ticked, in its configured
        // order, until enough results are resolved.
        let kind = kind_of(&seed);
        let providers = self.enabled_remote_providers(&seed, kind).await;
        let seed_provider_ids = if providers.is_empty() {
            HashMap::new()
        } else {
            self.repo.provider_ids(item_id).await.unwrap_or_default()
        };
        for (index, provider) in providers.into_iter().enumerate() {
            if scored.len() >= usize::try_from(wanted.max(0)).unwrap_or(0) {
                break;
            }
            let order = index + 1;
            let query = SimilarItemsQuery {
                user_id,
                limit: Some(wanted - i32::try_from(scored.len()).unwrap_or(0)),
                exclude_item_ids: taken.iter().copied().collect(),
                exclude_artist_ids: exclude_artist_ids.to_vec(),
            };
            let references = self
                .remote_references(provider.as_ref(), &seed, &seed_provider_ids, &query)
                .await;
            if references.is_empty() {
                continue;
            }
            scored.extend(
                self.resolve_remote_references(&references, order, kind, &mut taken)
                    .await,
            );
        }

        // Highest score first. The sort is deliberately left STABLE with no
        // tie-break: the score formula clamps to 1.0, so a provider's first few
        // results all tie, and their insertion order is the provider's own
        // ranking. C# relies on the same stability (`OrderByDescending` is a
        // stable sort in .NET) — adding a tie-break here would scramble the
        // most relevant results.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(usize::try_from(wanted.max(0)).unwrap_or(0));
        Ok(scored.into_iter().map(|(entity, _)| entity).collect())
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
    use std::time::Duration;

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
        seed_movie_in(db, id, name, genres, None).await;
    }

    /// [`seed_movie`] under a collection folder, so the per-library similarity
    /// provider selection resolves.
    async fn seed_movie_in(
        db: &Database,
        id: Uuid,
        name: &str,
        genres: &str,
        library: Option<Uuid>,
    ) {
        let movie = ferrofin_db::entities::base_items::BaseItemEntity {
            id: id.to_string(),
            type_: stored_type_name(BaseItemKind::Movie)
                .expect("movie type name")
                .to_owned(),
            name: Some(name.to_owned()),
            genres: Some(genres.to_owned()),
            top_parent_id: library.map(|l| l.to_string()),
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

    /// A remote provider returning a fixed reference list, recording whether it
    /// was asked.
    struct FakeRemote {
        name: &'static str,
        kind: BaseItemKind,
        references: Vec<SimilarItemReference>,
        cache: Option<Duration>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl RemoteSimilarItemsProvider for FakeRemote {
        fn name(&self) -> &str {
            self.name
        }
        fn supports(&self, item_kind: BaseItemKind) -> bool {
            item_kind == self.kind
        }
        fn cache_duration(&self) -> Option<Duration> {
            self.cache
        }
        async fn get_similar_items(
            &self,
            _seed: &BaseItemEntity,
            _seed_provider_ids: &HashMap<String, String>,
            _query: &SimilarItemsQuery,
        ) -> Vec<SimilarItemReference> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.references.clone()
        }
    }

    /// A virtual-folder manager serving one library whose `Movie` type options
    /// list `providers` as its similarity providers.
    struct FakeFolders {
        library_id: Uuid,
        providers: Vec<String>,
    }

    #[async_trait]
    impl ferrofin_traits::library::VirtualFolderManager for FakeFolders {
        async fn get_virtual_folders(
            &self,
        ) -> Result<Vec<ferrofin_model::entities_media::VirtualFolderInfo>, ServiceError> {
            Ok(vec![ferrofin_model::entities_media::VirtualFolderInfo {
                item_id: Some(self.library_id.to_string()),
                library_options: Some(ferrofin_model::configuration::LibraryOptions {
                    type_options: vec![ferrofin_model::configuration::TypeOptions {
                        type_: Some("Movie".to_owned()),
                        similar_item_providers: self.providers.clone(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }])
        }
        async fn add_virtual_folder(
            &self,
            _name: &str,
            _collection_type: Option<ferrofin_model::entities::CollectionTypeOptions>,
            _options: &ferrofin_model::configuration::LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn remove_virtual_folder(&self, _name: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn rename_virtual_folder(
            &self,
            _name: &str,
            _new_name: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn add_media_path(
            &self,
            _virtual_folder_name: &str,
            _path_info: &ferrofin_model::configuration::MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn update_media_path(
            &self,
            _virtual_folder_name: &str,
            _path_info: &ferrofin_model::configuration::MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn remove_media_path(
            &self,
            _virtual_folder_name: &str,
            _path: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn update_library_options(
            &self,
            _virtual_folder_name: &str,
            _options: &ferrofin_model::configuration::LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
    }

    // The C# score formula, verbatim: a provider-supplied score wins, else the
    // position decays it, and an earlier provider gets a small boost.
    #[test]
    fn scores_follow_the_upstream_formula() {
        // Local provider (order 0), first result: 1.0 - 0 + 10*0.005, clamped.
        assert!((calculate_score(None, 0, 0) - 1.0).abs() < f32::EPSILON);
        // Fifth result of provider order 1: 1.0 - 4*0.02 + 9*0.005.
        let expected = 1.0 - 4.0 * 0.02 + 9.0 * 0.005;
        assert!((calculate_score(None, 1, 4) - expected).abs() < 1e-6);
        // A provider-supplied score replaces the position decay.
        let expected = 0.5 + 9.0 * 0.005;
        assert!((calculate_score(Some(0.5), 1, 4) - expected).abs() < 1e-6);
        // Never outside 0..=1.
        assert!((calculate_score(Some(9.0), 0, 0) - 1.0).abs() < f32::EPSILON);
        assert!(calculate_score(Some(0.0), 30, 0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn a_remote_provider_a_library_did_not_tick_never_runs() {
        let db = test_db().await;
        let seed = Uuid::from_u128(0x901);
        seed_movie(&db, seed, "Alien", "SciFi").await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mgr = manager(&db).with_remote_providers(
            vec![Arc::new(FakeRemote {
                name: "TheMovieDb",
                kind: BaseItemKind::Movie,
                references: Vec::new(),
                cache: None,
                calls: Arc::clone(&calls),
            })],
            Arc::new(FakeFolders {
                library_id: Uuid::from_u128(0x900),
                // Saved options that do NOT list the provider.
                providers: vec!["Local Genre/Tag".to_owned()],
            }),
        );
        mgr.get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_ticked_remote_providers_matches_join_the_results() {
        let db = test_db().await;
        let library = Uuid::from_u128(0x910);
        let seed = Uuid::from_u128(0x911);
        let remote_match = Uuid::from_u128(0x912);
        seed_movie_in(&db, seed, "Alien", "SciFi", Some(library)).await;
        // No genre overlap, so only the remote provider can surface it.
        seed_movie_in(&db, remote_match, "Solaris", "Drama", Some(library)).await;
        FerrofinItemPersistenceService::new(db.clone())
            .save_provider_id(remote_match, "Tmdb", "348")
            .await
            .expect("save id");

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mgr = manager(&db).with_remote_providers(
            vec![Arc::new(FakeRemote {
                name: "TheMovieDb",
                kind: BaseItemKind::Movie,
                references: vec![SimilarItemReference {
                    provider_name: "Tmdb".to_owned(),
                    provider_id: "348".to_owned(),
                    score: None,
                }],
                cache: None,
                calls: Arc::clone(&calls),
            })],
            Arc::new(FakeFolders {
                library_id: library,
                providers: vec!["TheMovieDb".to_owned()],
            }),
        );
        let similar = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let names: Vec<_> = similar.iter().filter_map(|r| r.name.clone()).collect();
        assert!(
            names.contains(&"Solaris".to_owned()),
            "the remote match resolved into the results: {names:?}"
        );
    }

    #[tokio::test]
    async fn a_reference_that_matches_no_library_item_is_dropped() {
        let db = test_db().await;
        let library = Uuid::from_u128(0x920);
        let seed = Uuid::from_u128(0x921);
        seed_movie_in(&db, seed, "Alien", "SciFi", Some(library)).await;
        let mgr = manager(&db).with_remote_providers(
            vec![Arc::new(FakeRemote {
                name: "TheMovieDb",
                kind: BaseItemKind::Movie,
                references: vec![SimilarItemReference {
                    provider_name: "Tmdb".to_owned(),
                    provider_id: "does-not-exist".to_owned(),
                    score: None,
                }],
                cache: None,
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })],
            Arc::new(FakeFolders {
                library_id: library,
                providers: vec!["TheMovieDb".to_owned()],
            }),
        );
        let similar = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert!(similar.is_empty());
    }

    #[tokio::test]
    async fn cached_references_are_reused_and_expire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("themoviedb-similar-movie").join("x.json");
        let references = vec![SimilarItemReference {
            provider_name: "Tmdb".to_owned(),
            provider_id: "348".to_owned(),
            score: Some(0.5),
        }];
        super::write_reference_cache(&path, &references, Duration::from_mins(1));
        assert_eq!(
            super::read_reference_cache(&path).as_deref(),
            Some(&references[..])
        );

        // An expired entry is ignored rather than served stale.
        super::write_reference_cache(&path, &references, Duration::from_secs(0));
        assert_eq!(super::read_reference_cache(&path), None);
        // A missing file is simply a miss.
        assert_eq!(
            super::read_reference_cache(dir.path().join("nope.json").as_path()),
            None
        );
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
