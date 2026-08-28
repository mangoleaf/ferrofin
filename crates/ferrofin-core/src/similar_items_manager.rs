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
//! The local provider runs in two phases, as C# `MovieSimilarItemsProvider`
//! does: the score pass above, then **one** `InternalItemsQuery` over the scored
//! ids carrying the per-kind filter set of the C# provider that serves the seed
//! (`Movie`/`Trailer`: movie kinds — plus `Trailer`/`LiveTvProgram` when
//! `EnableExternalContentInSuggestions` — `IsMovie`, unplayed only; `Series`;
//! `MusicAlbum`/`MusicArtist`/`Audio` honouring `ExcludeArtistIds`;
//! `LiveTvProgram` by its movie/series flag) and the user's library access, so
//! the watch-state and access rules are the query layer's, not a second copy.
//! A kind C# has no local provider for scores nothing, and the controller's
//! `Episode` / by-name short-circuit lives in [`SimilarItemsManager::get_similar_items`].
//!
//! Accepted divergences from C#: one weighted scorer serves every kind (C#'s
//! `Series`/music/Live TV providers run a plain genre-or-tag match in random
//! order); `IsFavoriteOrLiked` is approximated as favorite-only (as elsewhere in
//! the query layer); the person-recommendation IMDb de-dup is dropped; and ties
//! are broken **deterministically** (`SortName`, then `Id`) rather than by C#'s
//! `Random`, so results are stable.

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

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{
    RemoteSimilarItemsProvider, SimilarItemReference, SimilarItemsManager, SimilarItemsQuery,
    SimilarItemsRecommendation,
};
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::persistence::ItemRepository;

use crate::item_type_lookup::stored_type_name;
use crate::kinds::supports_similarity;
use crate::similar_items_repository::SimilarItemsRepository;

/// Reads an unexpired reference cache, or `None` when it is missing, stale or
/// unparseable.
fn read_reference_cache(path: &Path) -> Option<Vec<SimilarItemReference>> {
    // A missing entry is the normal cache miss and says nothing; an entry that
    // exists but will not parse is worth one warning (C# logs the same).
    let raw = std::fs::read_to_string(path).ok()?;
    let cache: SimilarItemsCache = match serde_json::from_str(&raw) {
        Ok(cache) => cache,
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "similar-items cache is unreadable");
            return None;
        }
    };
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
    if let Ok(json) = serde_json::to_string(&cache)
        && let Err(err) = std::fs::write(path, json)
    {
        tracing::warn!(%err, path = %path.display(), "could not write similar-items cache");
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

/// The ids and presentation keys already spoken for, seeded with the item the
/// request is about.
///
/// C# keeps `excludeIds` AND `excludeKeys` (a `PresentationUniqueKey` set,
/// case-insensitive) and admits a result only when it is new to BOTH — that is
/// what stops a 4K and a 1080p row for the same film both showing up as
/// "similar".
struct Seen {
    ids: std::collections::HashSet<Uuid>,
    keys: std::collections::HashSet<String>,
}

impl Seen {
    /// The exclusion set for a request about `item_id`, whose own presentation
    /// key is `key`.
    fn new(item_id: Uuid, key: Option<&str>) -> Self {
        Self {
            ids: std::collections::HashSet::from([item_id]),
            keys: key.map(presentation_key).into_iter().collect(),
        }
    }

    /// Whether `entity` is new to both sets, claiming it when it is.
    fn claim(&mut self, id: Uuid, entity: &BaseItemEntity) -> bool {
        // Both `Add`s run in C# before the `&&`, so a rejected-on-id row still
        // burns its key. Matching that keeps the two implementations' output
        // identical on a duplicate-heavy library.
        let new_id = self.ids.insert(id);
        let new_key = entity
            .presentation_unique_key
            .as_deref()
            .is_none_or(|key| self.keys.insert(presentation_key(key)));
        new_id && new_key
    }

    /// The ids to hand a provider as `ExcludeItemIds`.
    fn ids(&self) -> Vec<Uuid> {
        self.ids.iter().copied().collect()
    }
}

/// A presentation key normalized for the case-insensitive comparison C# uses.
fn presentation_key(key: &str) -> String {
    key.to_lowercase()
}

/// Which providers serve one seed, and where the local scorer sits among them.
struct SimilarityPlan {
    /// The enabled remote providers, in the library's configured order.
    remote: Vec<Arc<dyn RemoteSimilarItemsProvider>>,
    /// The local scorer's position in that same order; `0` when the library
    /// expressed no preference.
    local_order: usize,
}

impl SimilarityPlan {
    /// No remote provider runs, so the local scorer is the whole plan.
    fn local_only() -> Self {
        Self {
            remote: Vec::new(),
            local_order: 0,
        }
    }
}

/// A provider's position in a configured order list — port of
/// `GetConfiguredSimilarProviderOrder`, which sorts an unlisted provider LAST
/// but leaves everything first when no order was configured at all.
fn provider_rank(order: &[String], name: &str) -> usize {
    if order.is_empty() {
        return 0;
    }
    order
        .iter()
        .position(|n| n.eq_ignore_ascii_case(name))
        .unwrap_or(usize::MAX)
}

/// The display name of the built-in local similarity scorer — the string a
/// library's `SimilarItemProviders` order lists it under. Must match the name
/// `ferrofin-providers`' library-options registry advertises.
const LOCAL_SIMILARITY_PROVIDER: &str = "Local Genre/Tag";

/// The default number of similar items returned when the caller gives no limit.
const DEFAULT_SIMILAR_LIMIT: i32 = 10;

/// How many scored candidates the score pass keeps per result wanted, so the
/// access/played filter that follows can drop rows without under-filling the
/// page (C# `MovieSimilarItemsProvider`: `.Take(limit * 3)`).
const CANDIDATE_OVERSAMPLE: i32 = 3;

/// The filter a C# local similarity provider applies to its candidates — phase
/// 2 of `MovieSimilarItemsProvider.GetBatchSimilarItemsAsync`, or the
/// `InternalItemsQuery` the `Series`/`MusicAlbum`/`MusicArtist`/`Audio`/
/// `LiveTvProgram` providers build. The user's library access is added on top
/// by [`FerrofinSimilarItemsManager::configure_user_access`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct LocalFilter {
    /// The kinds a result may be (C# `IncludeItemTypes`); empty when no C#
    /// provider serves the seed's kind, in which case nothing is scored.
    include_item_types: Vec<BaseItemKind>,
    /// C# `IsMovie` (the movie provider sets it; the others leave it null).
    is_movie: Option<bool>,
    /// Whether only unplayed rows qualify (the movie provider's
    /// `IsPlayed = false`). Only meaningful with a user.
    unplayed_only: bool,
    /// Whether the request's `ExcludeArtistIds` apply (the music providers).
    honours_exclude_artist_ids: bool,
}

/// One similar-items request as the local provider sees it.
struct LocalRequest<'a> {
    /// The seed row.
    seed: &'a BaseItemEntity,
    /// The seed's id, parsed.
    seed_id: Uuid,
    /// The seed's kind.
    kind: BaseItemKind,
    /// The request's `ExcludeArtistIds`.
    exclude_artist_ids: &'a [Uuid],
    /// The requesting user, when the request is user-scoped.
    user_id: Option<Uuid>,
    /// How many results the request wants in all.
    wanted: i32,
}

impl LocalFilter {
    /// A provider that only restricts the result kinds (`Series`, Live TV).
    fn of_kinds(include_item_types: Vec<BaseItemKind>) -> Self {
        Self {
            include_item_types,
            ..Self::default()
        }
    }

    /// A music provider: one result kind, and the caller's artist exclusions.
    fn music(kind: BaseItemKind) -> Self {
        Self {
            include_item_types: vec![kind],
            honours_exclude_artist_ids: true,
            ..Self::default()
        }
    }
}

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
    /// Reads `EnableExternalContentInSuggestions`, which widens movie
    /// suggestions to trailers and Live TV programs. Absent → Jellyfin's
    /// default for that setting (`true`).
    configuration: Option<Arc<dyn ServerConfigurationManager>>,
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
            configuration: None,
        }
    }

    /// Reads the server configuration through `configuration` — the
    /// `EnableExternalContentInSuggestions` switch that lets trailers and Live
    /// TV programs stand in as "similar movies" (composition root only).
    #[must_use]
    pub fn with_configuration(
        mut self,
        configuration: Arc<dyn ServerConfigurationManager>,
    ) -> Self {
        self.configuration = Some(configuration);
        self
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

    /// The similarity plan for one seed: the remote providers this library
    /// enabled for `kind`, in the admin's configured order, plus where the
    /// local scorer sits in that same order.
    ///
    /// Port of the `TypeOptions.SimilarItemProviders` /
    /// `SimilarItemProviderOrder` resolution in `SimilarItemsManager`, which
    /// sorts local and remote providers as ONE list. Reads the library exactly
    /// once, and not at all when no compiled provider serves this kind.
    async fn similarity_plan(&self, seed: &BaseItemEntity, kind: BaseItemKind) -> SimilarityPlan {
        // Resolving the library reads the filesystem, so narrow to the
        // providers that could serve this kind at all first — most kinds have
        // none, and those must not pay for a `get_virtual_folders` call.
        let (Some(library), false) = (self.library.as_ref(), self.remote.is_empty()) else {
            return SimilarityPlan::local_only();
        };
        let candidates: Vec<&Arc<dyn RemoteSimilarItemsProvider>> =
            self.remote.iter().filter(|p| p.supports(kind)).collect();
        if candidates.is_empty() {
            return SimilarityPlan::local_only();
        }
        let Some(type_name) = seed.type_.rsplit('.').next() else {
            return SimilarityPlan::local_only();
        };
        // The row's `TopParentId` is stored in the DB GUID form (uppercase,
        // hyphenated) while `VirtualFolderInfo.item_id` is the display form
        // (lowercase). Compare as parsed `Uuid`s, never as bytes.
        let Some(top_parent) = seed
            .top_parent_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            return SimilarityPlan::local_only();
        };
        let Ok(folders) = library.get_virtual_folders().await else {
            return SimilarityPlan::local_only();
        };
        let options = folders
            .iter()
            .find(|f| {
                f.item_id.as_deref().and_then(|id| Uuid::parse_str(id).ok()) == Some(top_parent)
            })
            .and_then(|f| f.library_options.as_ref())
            .and_then(|o| {
                o.type_options.iter().find(|t| {
                    t.type_
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case(type_name))
                })
            });
        let Some(options) = options else {
            // No saved selection: remote similarity is opt-in, so nothing runs
            // and the local scorer is the only provider there is.
            return SimilarityPlan::local_only();
        };
        let order = if options.similar_item_provider_order.is_empty() {
            &options.similar_item_providers
        } else {
            &options.similar_item_provider_order
        };
        let mut remote: Vec<Arc<dyn RemoteSimilarItemsProvider>> = candidates
            .into_iter()
            .filter(|p| {
                options
                    .similar_item_providers
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(p.name()))
            })
            .map(Arc::clone)
            .collect();
        remote.sort_by_key(|p| provider_rank(order, p.name()));
        SimilarityPlan {
            // C# `GetConfiguredSimilarProviderOrder` returns `int.MaxValue` for
            // a provider absent from a non-empty order list, i.e. LAST — not
            // first. An admin who lists only TheMovieDb has unticked the local
            // box, and local must not then jump ahead of it.
            local_order: provider_rank(order, LOCAL_SIMILARITY_PROVIDER),
            remote,
        }
    }

    /// The kinds a "similar movie" may be: `Movie`, plus `Trailer` and
    /// `LiveTvProgram` when `EnableExternalContentInSuggestions` is on (the
    /// list C# `MovieSimilarItemsProvider` and `GetMovieRecommendationsAsync`
    /// both build). Unconfigured, the setting takes Jellyfin's default, `true`.
    async fn movie_candidate_kinds(&self) -> Vec<BaseItemKind> {
        let mut kinds = vec![BaseItemKind::Movie];
        let external = match self.configuration.as_ref() {
            None => true,
            Some(configuration) => match configuration.configuration().await {
                Ok(config) => config.enable_external_content_in_suggestions,
                Err(err) => {
                    tracing::warn!(%err, "reading the server configuration failed; assuming its default");
                    true
                }
            },
        };
        if external {
            kinds.push(BaseItemKind::Trailer);
            kinds.push(BaseItemKind::LiveTvProgram);
        }
        kinds
    }

    /// The filter set of the C# local provider that serves `seed` —
    /// `ILocalSimilarItemsProvider.Supports(type)` resolved per kind, with each
    /// provider's own `InternalItemsQuery` shape. A kind none of them serves
    /// gets an empty filter, i.e. no local results.
    async fn local_filter(&self, seed: &BaseItemEntity, kind: BaseItemKind) -> LocalFilter {
        match kind {
            // `MovieSimilarItemsProvider` (Movie + Trailer seeds): movie kinds,
            // `IsMovie = true`, `IsPlayed = false`.
            BaseItemKind::Movie | BaseItemKind::Trailer => LocalFilter {
                include_item_types: self.movie_candidate_kinds().await,
                is_movie: Some(true),
                unplayed_only: true,
                honours_exclude_artist_ids: false,
            },
            BaseItemKind::Series => LocalFilter::of_kinds(vec![BaseItemKind::Series]),
            BaseItemKind::MusicAlbum => LocalFilter::music(BaseItemKind::MusicAlbum),
            BaseItemKind::MusicArtist => LocalFilter::music(BaseItemKind::MusicArtist),
            // `AudioBook : Audio`, so the audio provider's `Supports` admits it.
            BaseItemKind::Audio | BaseItemKind::AudioBook => {
                LocalFilter::music(BaseItemKind::Audio)
            }
            // `LiveTvProgramSimilarItemsProvider` picks the list by the
            // program's own flags; it sets neither `IsMovie` nor `IsPlayed`.
            BaseItemKind::LiveTvProgram if seed.is_movie => {
                LocalFilter::of_kinds(self.movie_candidate_kinds().await)
            }
            BaseItemKind::LiveTvProgram if seed.is_series => {
                LocalFilter::of_kinds(vec![BaseItemKind::Series])
            }
            BaseItemKind::LiveTvProgram => LocalFilter::of_kinds(vec![BaseItemKind::LiveTvProgram]),
            _ => LocalFilter::default(),
        }
    }

    /// Scopes `query` to `user_id` the way C# `ConfigureUserAccess`
    /// (`AddUserToQuery`) does: the user goes on the query, which is what the
    /// watch-state predicates key on, and a user who may not see every library
    /// is confined to the `TopParentId`s they may. Returns `false` when the
    /// user can see no library at all — C# reaches for a `Guid.NewGuid()`
    /// scope there so the query matches nothing; the caller skips it instead.
    ///
    /// An id that resolves to no user leaves the query unscoped, as the C#
    /// controller hands a null user through.
    async fn configure_user_access(
        &self,
        query: &mut InternalItemsQuery,
        user_id: Uuid,
    ) -> Result<bool, ServiceError> {
        let Some(user) = self.repo.fetch_user(user_id).await? else {
            return Ok(true);
        };
        if let Some(scope) = self.repo.accessible_top_parents(&user).await? {
            if scope.is_empty() {
                return Ok(false);
            }
            query.top_parent_ids = scope;
        }
        query.set_user(user);
        Ok(true)
    }

    /// The local provider's results for `seed`, in score order, over-sampled
    /// for `wanted`: the score pass, then ONE access/played query over the
    /// scored ids (C# phases 1–2 of `GetBatchSimilarItemsAsync`). The caller
    /// takes its fill after the presentation-key de-dup, as C# `DistinctBy`
    /// runs before `Take(limit)`.
    ///
    /// `exclude_ids` is handed to the score pass, so its limit yields that
    /// many *new* rows; the filter pass carries `ExcludeArtistIds` (for the
    /// kinds whose provider honours it), the kind set, `IsMovie`/`IsPlayed`
    /// and the user's access, all through the shared query translator.
    async fn local_similar(
        &self,
        request: &LocalRequest<'_>,
        exclude_ids: &[Uuid],
        wanted: i32,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if wanted <= 0 {
            return Ok(Vec::new());
        }
        let filter = self.local_filter(request.seed, request.kind).await;
        let candidate_types: Vec<&str> = filter
            .include_item_types
            .iter()
            .copied()
            .filter_map(stored_type_name)
            .collect();
        let mut candidates = self
            .repo
            .weighted_similar_items(
                request.seed_id,
                &candidate_types,
                exclude_ids,
                wanted.saturating_mul(CANDIDATE_OVERSAMPLE),
            )
            .await?;
        if candidates.is_empty() {
            return Ok(candidates);
        }

        let mut access = InternalItemsQuery {
            item_ids: candidates
                .iter()
                .filter_map(|c| Uuid::parse_str(&c.id).ok())
                .collect(),
            include_item_types: filter.include_item_types.clone(),
            is_movie: filter.is_movie,
            is_played: filter.unplayed_only.then_some(false),
            exclude_artist_ids: if filter.honours_exclude_artist_ids {
                request.exclude_artist_ids.to_vec()
            } else {
                Vec::new()
            },
            ..InternalItemsQuery::default()
        };
        if let Some(user_id) = request.user_id
            && !self.configure_user_access(&mut access, user_id).await?
        {
            return Ok(Vec::new());
        }
        let accessible: std::collections::HashSet<Uuid> = self
            .items
            .get_item_ids(&access)
            .await?
            .into_iter()
            .collect();
        // Phase 3: the filter says which survive; the score pass says in
        // what order.
        candidates.retain(|c| Uuid::parse_str(&c.id).is_ok_and(|id| accessible.contains(&id)));
        Ok(candidates)
    }

    /// Runs the local provider and appends its results at `provider_order`,
    /// skipping anything already taken.
    async fn push_local_results(
        &self,
        request: &LocalRequest<'_>,
        provider_order: usize,
        claimed: &mut Seen,
        scored: &mut Vec<(BaseItemEntity, f32)>,
    ) -> Result<(), ServiceError> {
        let remaining = request.wanted - i32::try_from(scored.len()).unwrap_or(0);
        if remaining <= 0 {
            return Ok(());
        }
        // C# hands the local provider `ExcludeItemIds`, so its `Limit` yields
        // that many *new* rows. Filtering in Rust instead would let each
        // duplicate burn a slot and under-fill the requested limit.
        let exclude: Vec<Uuid> = claimed.ids();
        let by_overlap = self.local_similar(request, &exclude, remaining).await?;
        let wanted_len = usize::try_from(request.wanted).unwrap_or(0);
        for (position, entity) in by_overlap.into_iter().enumerate() {
            if scored.len() >= wanted_len {
                break;
            }
            if let Ok(id) = Uuid::parse_str(&entity.id)
                && claimed.claim(id, &entity)
            {
                scored.push((entity, calculate_score(None, provider_order, position)));
            }
        }
        Ok(())
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
        claimed: &mut Seen,
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

        // Keyed in first-seen order, not `HashMap` order: scores saturate at
        // 1.0 for the first few positions, so an arbitrary iteration order
        // would make two identical requests answer differently.
        let mut by_provider: Vec<(&str, Vec<String>)> = Vec::new();
        for reference in references {
            match by_provider
                .iter_mut()
                .find(|(name, _)| *name == reference.provider_name.as_str())
            {
                Some((_, values)) => values.push(reference.provider_id.clone()),
                None => by_provider.push((
                    reference.provider_name.as_str(),
                    vec![reference.provider_id.clone()],
                )),
            }
        }

        // C# collects into `resolvedByKey`, a presentation-key-keyed map that
        // keeps the HIGHEST-scoring row per key, and only writes the winners
        // into the exclude sets once the whole batch is resolved. Claiming as
        // rows arrive instead would let an arbitrary DB row order decide which
        // copy of a film represents it.
        let mut resolved: Vec<(String, Uuid, BaseItemEntity, f32)> = Vec::new();
        for (provider_key, values) in by_provider {
            let rows = match self
                .repo
                .items_with_provider_values(provider_key, &values)
                .await
            {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::warn!(
                        %err,
                        provider = provider_key,
                        "resolving similar-item references failed"
                    );
                    continue;
                }
            };
            for (item_id, value) in rows {
                let key = (provider_key.to_lowercase(), value.to_lowercase());
                let Some(&(score, position)) = best.get(&key) else {
                    continue;
                };
                if claimed.ids.contains(&item_id) {
                    continue;
                }
                let Ok(Some(entity)) = self.items.retrieve_item(item_id).await else {
                    continue;
                };
                if kind_of(&entity) != kind {
                    continue;
                }
                // C# `GetPresentationUniqueKey()` falls back to the item id, so
                // a keyless row is its own bucket rather than colliding with
                // every other keyless row.
                let key = entity
                    .presentation_unique_key
                    .as_deref()
                    .map_or_else(|| item_id.to_string(), presentation_key);
                if claimed.keys.contains(&key) {
                    continue;
                }
                let score = calculate_score(score, provider_order, position);
                match resolved.iter_mut().find(|(seen, ..)| *seen == key) {
                    Some(entry) if entry.3 < score => *entry = (key, item_id, entity, score),
                    Some(_) => {}
                    None => resolved.push((key, item_id, entity, score)),
                }
            }
        }
        let mut out = Vec::with_capacity(resolved.len());
        for (key, item_id, entity, score) in resolved {
            claimed.ids.insert(item_id);
            claimed.keys.insert(key);
            out.push((entity, score));
        }
        out
    }

    /// Builds a "similar to `seed`" category, or `None` when the seed has no
    /// similar items (C# skips empty baselines).
    async fn similar_category(
        &self,
        seed: &BaseItemEntity,
        user_id: Uuid,
        recommendation_type: RecommendationType,
        item_limit: i32,
        dto_options: &DtoOptions,
    ) -> Result<Option<SimilarItemsRecommendation>, ServiceError> {
        let Ok(seed_id) = Uuid::parse_str(&seed.id) else {
            return Ok(None);
        };
        // Recommendations use the LOCAL provider only. C#
        // `GetSimilarItemsRecommendationsAsync` resolves an
        // `IBatchLocalSimilarItemsProvider` and calls that alone — remote
        // providers never take part, so a category never fans out to TMDB
        // once per baseline — and hands it the user, so the movie provider's
        // unplayed/access filter applies here too.
        let _ = dto_options;
        let request = LocalRequest {
            seed,
            seed_id,
            kind: kind_of(seed),
            exclude_artist_ids: &[],
            user_id: Some(user_id),
            wanted: item_limit,
        };
        let mut claimed = Seen::new(seed_id, seed.presentation_unique_key.as_deref());
        let mut items = Vec::new();
        let item_limit_len = usize::try_from(item_limit).unwrap_or(0);
        for entity in self.local_similar(&request, &[seed_id], item_limit).await? {
            if items.len() >= item_limit_len {
                break;
            }
            // C# `DistinctBy(PresentationUniqueKey)`: one row per film.
            if let Ok(id) = Uuid::parse_str(&entity.id)
                && claimed.claim(id, &entity)
            {
                items.push(entity);
            }
        }
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

    /// One "similar to" category per baseline `seed`, skipping the seeds with
    /// no similar items (C# `GetSimilarItemsRecommendationsAsync`).
    async fn similar_categories(
        &self,
        seeds: impl Iterator<Item = BaseItemEntity> + Send,
        user_id: Uuid,
        recommendation_type: RecommendationType,
        item_limit: i32,
        dto_options: &DtoOptions,
    ) -> Result<Vec<SimilarItemsRecommendation>, ServiceError> {
        let mut out = Vec::new();
        for seed in seeds {
            if let Some(rec) = self
                .similar_category(&seed, user_id, recommendation_type, item_limit, dto_options)
                .await?
            {
                out.push(rec);
            }
        }
        Ok(out)
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
        let movie_kinds = self.movie_candidate_kinds().await;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let mut query = InternalItemsQuery {
                include_item_types: movie_kinds.clone(),
                is_movie: Some(true),
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
        let kind = kind_of(&seed);
        // C# `LibraryController.GetSimilarItems`: an `Episode`, or an
        // `IItemByName` other than a `MusicArtist`, answers with an empty
        // result before any provider runs.
        if !supports_similarity(kind) {
            return Ok(Vec::new());
        }
        let wanted = limit.unwrap_or(DEFAULT_SIMILAR_LIMIT);
        // The local scorer is always enabled, but it takes its place in the
        // SAME order list as the remote providers (C# puts local and remote
        // into one `matchingProviders` list and sorts the lot by
        // `SimilarItemProviderOrder`), so a library that ranks TheMovieDb
        // above "Local Genre/Tag" really does get TMDB's results first.
        let SimilarityPlan {
            remote: providers,
            local_order,
        } = self.similarity_plan(&seed, kind).await;
        let remote_count = providers.len();
        let seed_provider_ids = if providers.is_empty() {
            HashMap::new()
        } else {
            self.repo.provider_ids(item_id).await.unwrap_or_default()
        };

        let local = LocalRequest {
            seed: &seed,
            seed_id: item_id,
            kind,
            exclude_artist_ids,
            user_id,
            wanted,
        };
        let mut claimed = Seen::new(item_id, seed.presentation_unique_key.as_deref());
        let mut scored: Vec<(BaseItemEntity, f32)> = Vec::new();
        let mut ran_local = false;
        let wanted_len = usize::try_from(wanted.max(0)).unwrap_or(0);

        for (index, provider) in providers.into_iter().enumerate() {
            // Run the local scorer once, at its configured position.
            if !ran_local && local_order <= index {
                ran_local = true;
                self.push_local_results(&local, index, &mut claimed, &mut scored)
                    .await?;
            }
            if scored.len() >= wanted_len {
                break;
            }
            // This provider's position in the COMBINED list: it shifts down by
            // one only once the local scorer has taken a slot ahead of it.
            let order = index + usize::from(ran_local);
            let query = SimilarItemsQuery {
                user_id,
                limit: Some(wanted - i32::try_from(scored.len()).unwrap_or(0)),
                exclude_item_ids: claimed.ids(),
                exclude_artist_ids: exclude_artist_ids.to_vec(),
            };
            let references = self
                .remote_references(provider.as_ref(), &seed, &seed_provider_ids, &query)
                .await;
            if references.is_empty() {
                continue;
            }
            scored.extend(
                self.resolve_remote_references(&references, order, kind, &mut claimed)
                    .await,
            );
        }
        if !ran_local {
            // Local ranked last: its position in the combined list is the
            // number of remote providers ahead of it, which still earns the
            // rank boost C# gives it — `usize::MAX` would zero it out.
            self.push_local_results(&local, remote_count, &mut claimed, &mut scored)
                .await?;
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
            include_item_types: self.movie_candidate_kinds().await,
            is_movie: Some(true),
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
        let similar_to_played = self
            .similar_categories(
                recently_played.into_iter().take(cat_limit),
                uid,
                RecommendationType::SimilarToRecentlyPlayed,
                item_limit,
                dto_options,
            )
            .await?;
        let similar_to_liked = self
            .similar_categories(
                liked.into_iter().take(cat_limit),
                uid,
                RecommendationType::SimilarToLikedItem,
                item_limit,
                dto_options,
            )
            .await?;
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
    use crate::test_support::{
        seed_item_genre, seed_item_value, seed_named_user, seed_user, seed_user_data, test_db,
    };
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
        seed_movie_keyed(db, id, name, genres, library, None).await;
    }

    /// `seed_movie_in`, plus the `PresentationUniqueKey` two rows for the same
    /// film share.
    async fn seed_movie_keyed(
        db: &Database,
        id: Uuid,
        name: &str,
        genres: &str,
        library: Option<Uuid>,
        presentation_unique_key: Option<&str>,
    ) {
        let movie = ferrofin_db::entities::base_items::BaseItemEntity {
            presentation_unique_key: presentation_unique_key.map(str::to_owned),
            // The DB GUID form, as every writer in the crate uses — a display
            // form here only agrees with the rest of the schema for ids that
            // happen to contain no hex letters.
            id: ferrofin_db::store::guid_to_db(id),
            type_: stored_type_name(BaseItemKind::Movie)
                .expect("movie type name")
                .to_owned(),
            name: Some(name.to_owned()),
            genres: Some(genres.to_owned()),
            // The scanner writes this through `guid_to_db`, i.e. the uppercase
            // hyphenated DB form — NOT the lowercase display form. A test that
            // seeds the display form hides any format mismatch in the lookup.
            top_parent_id: library.map(ferrofin_db::store::guid_to_db),
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

    /// Gives `user` the `EnableAllFolders` permission every Jellyfin-created
    /// user holds by default (`seed_user` writes the bare row only).
    async fn grant_all_folders(db: &Database, user: &UserEntity) {
        crate::user_entity_ext::set_permission(
            db.writer(),
            &user.id,
            ferrofin_db::enums::PermissionKind::EnableAllFolders,
            true,
        )
        .await
        .expect("grant EnableAllFolders");
    }

    /// Confines `user` to `folders` (`EnableAllFolders = false`, `EnabledFolders
    /// = folders`) — the dashboard's "Library access" checkboxes.
    async fn restrict_to_folders(db: &Database, user: &UserEntity, folders: &[Uuid]) {
        crate::user_entity_ext::set_permission(
            db.writer(),
            &user.id,
            ferrofin_db::enums::PermissionKind::EnableAllFolders,
            false,
        )
        .await
        .expect("revoke EnableAllFolders");
        let ids: Vec<String> = folders.iter().map(ToString::to_string).collect();
        crate::user_entity_ext::set_preference(
            db.writer(),
            &user.id,
            ferrofin_db::enums::PreferenceKind::EnabledFolders,
            &ids,
        )
        .await
        .expect("set EnabledFolders");
    }

    /// Seeds a `kind` row named `name` with `genres` attached through
    /// `ItemValues`, for the non-movie providers.
    async fn seed_kind(db: &Database, id: Uuid, kind: BaseItemKind, name: &str, genres: &str) {
        let row = ferrofin_db::entities::base_items::BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: stored_type_name(kind).expect("type name").to_owned(),
            name: Some(name.to_owned()),
            clean_name: Some(crate::text_util::get_clean_value(name)),
            genres: Some(genres.to_owned()),
            ..Default::default()
        };
        FerrofinItemPersistenceService::new(db.clone())
            .save_items(&[row])
            .await
            .expect("seed item");
        for genre in genres.split('|').filter(|g| !g.is_empty()) {
            seed_item_genre(db, id, genre).await;
        }
    }

    /// Attaches an `AlbumArtist` item value to `item` (what the scanner writes
    /// for an album's artist), appending to its existing values.
    async fn seed_album_artist(db: &Database, item: Uuid, artist: &str) {
        seed_item_value(
            db,
            item,
            ferrofin_db::enums::ItemValueType::AlbumArtist,
            artist,
        )
        .await;
    }

    /// The names of `rows`, in order.
    fn names(rows: &[BaseItemEntity]) -> Vec<String> {
        rows.iter().filter_map(|r| r.name.clone()).collect()
    }

    /// A configuration manager with one knob: `EnableExternalContentInSuggestions`.
    struct FakeConfig {
        external: bool,
    }

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unreachable!("application paths are not read by similar items")
        }
        async fn configuration(
            &self,
        ) -> Result<Arc<ferrofin_model::configuration::ServerConfiguration>, ServiceError> {
            let mut config = crate::configuration_manager::default_server_configuration();
            config.enable_external_content_in_suggestions = self.external;
            Ok(Arc::new(config))
        }
        async fn update_configuration(
            &self,
            _config: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
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
                // The real `VirtualFolderManager` reports the display form
                // (lowercase), while rows store the DB form — the two must
                // still resolve to the same library.
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
                library_id: Uuid::from_u128(0x9ab_cdd),
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
        let library = Uuid::from_u128(0x9ab_cde);
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
    async fn the_cache_path_matches_jellyfins_layout() {
        // `{cache}/{provider}-similar-{type}/{itemId:N}.json`, all lowercased —
        // a shared cache directory has to stay valid for both servers.
        let db = test_db().await;
        let id = Uuid::from_u128(0x930);
        let mgr = manager(&db).with_cache_dir(PathBuf::from("/cache"));
        let seed = ferrofin_db::entities::base_items::BaseItemEntity {
            id: id.to_string(),
            type_: stored_type_name(BaseItemKind::Movie)
                .expect("movie type name")
                .to_owned(),
            ..Default::default()
        };
        assert_eq!(
            mgr.cache_path("TheMovieDb", &seed),
            Some(PathBuf::from(format!(
                "/cache/themoviedb-similar-movie/{}.json",
                id.simple()
            )))
        );
    }

    #[tokio::test]
    async fn the_library_order_can_rank_a_remote_provider_above_the_local_scorer() {
        // A library that lists TheMovieDb before "Local Genre/Tag" must get
        // TMDB's match first, even though the local scorer also has results.
        let db = test_db().await;
        let library = Uuid::from_u128(0x9ab_ce0);
        let seed = Uuid::from_u128(0x941);
        let local_match = Uuid::from_u128(0x942);
        let remote_match = Uuid::from_u128(0x943);
        seed_movie_in(&db, seed, "Alien", "SciFi", Some(library)).await;
        seed_movie_in(&db, local_match, "Aliens", "SciFi", Some(library)).await;
        seed_movie_in(&db, remote_match, "Solaris", "Drama", Some(library)).await;
        FerrofinItemPersistenceService::new(db.clone())
            .save_provider_id(remote_match, "Tmdb", "348")
            .await
            .expect("save id");

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
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })],
            Arc::new(FakeFolders {
                library_id: library,
                providers: vec!["TheMovieDb".to_owned(), "Local Genre/Tag".to_owned()],
            }),
        );
        let names: Vec<_> = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), Some(1))
            .await
            .expect("similar")
            .iter()
            .filter_map(|r| r.name.clone())
            .collect();
        assert_eq!(
            names,
            ["Solaris"],
            "the remote provider was ranked first, so its match wins the single slot"
        );
    }

    #[tokio::test]
    async fn the_best_scoring_copy_represents_a_remote_match() {
        // C# `ResolveRemoteReferences` buckets the batch by PresentationUniqueKey
        // and keeps the HIGHEST-scoring row per bucket, writing the winners into
        // the exclude sets only once the whole batch is resolved. Keeping
        // whichever row the DB happened to return first would make the answer
        // depend on row order.
        let db = test_db().await;
        let library = Uuid::from_u128(0x9ab_da1);
        let seed = Uuid::from_u128(0x9ab_da2);
        let hd = Uuid::from_u128(0x9ab_da3);
        let uhd = Uuid::from_u128(0x9ab_da4);
        seed_movie_in(&db, seed, "Alien", "SciFi", Some(library)).await;
        seed_movie_keyed(&db, hd, "Aliens HD", "Drama", Some(library), Some("aliens")).await;
        seed_movie_keyed(
            &db,
            uhd,
            "Aliens UHD",
            "Drama",
            Some(library),
            Some("aliens"),
        )
        .await;
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        persistence
            .save_provider_id(hd, "Tmdb", "679-hd")
            .await
            .expect("save hd id");
        persistence
            .save_provider_id(uhd, "Tmdb", "679-uhd")
            .await
            .expect("save uhd id");

        let mgr = manager(&db).with_remote_providers(
            vec![Arc::new(FakeRemote {
                name: "TheMovieDb",
                kind: BaseItemKind::Movie,
                references: vec![
                    // The lower-scoring copy is listed FIRST, so first-wins and
                    // best-wins give different answers.
                    SimilarItemReference {
                        provider_name: "Tmdb".to_owned(),
                        provider_id: "679-hd".to_owned(),
                        score: Some(0.2),
                    },
                    SimilarItemReference {
                        provider_name: "Tmdb".to_owned(),
                        provider_id: "679-uhd".to_owned(),
                        score: Some(0.9),
                    },
                ],
                cache: None,
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })],
            Arc::new(FakeFolders {
                library_id: library,
                providers: vec!["TheMovieDb".to_owned()],
            }),
        );
        let names: Vec<_> = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), Some(10))
            .await
            .expect("similar")
            .iter()
            .filter_map(|r| r.name.clone())
            .collect();
        assert_eq!(names, ["Aliens UHD"], "the higher-scoring copy must win");
    }

    #[tokio::test]
    async fn two_rows_for_the_same_film_are_returned_once() {
        // C# admits a result only when it is new to BOTH `excludeIds` and
        // `excludeKeys`. A 4K and a 1080p copy of the same film share a
        // PresentationUniqueKey, so only one of them may appear.
        let db = test_db().await;
        let seed = Uuid::from_u128(0x9ab_cf1);
        let hd = Uuid::from_u128(0x9ab_cf2);
        let uhd = Uuid::from_u128(0x9ab_cf3);
        seed_movie_in(&db, seed, "Alien", "SciFi", None).await;
        seed_movie_keyed(&db, hd, "Aliens", "SciFi", None, Some("aliens-1986")).await;
        seed_movie_keyed(&db, uhd, "Aliens", "SciFi", None, Some("ALIENS-1986")).await;
        let names: Vec<_> = manager(&db)
            .get_similar_items(seed, &[], None, &DtoOptions::default(), Some(10))
            .await
            .expect("similar")
            .iter()
            .filter_map(|r| r.name.clone())
            .collect();
        assert_eq!(
            names,
            ["Aliens"],
            "the duplicate row must be dropped, case-insensitively"
        );
    }

    #[test]
    fn an_unlisted_provider_sorts_last_not_first() {
        // C# `GetConfiguredSimilarProviderOrder` returns int.MaxValue for a
        // provider missing from a NON-EMPTY order list. Returning 0 there put
        // the local scorer ahead of the very provider the admin listed.
        let configured = ["TheMovieDb".to_owned()];
        assert_eq!(super::provider_rank(&configured, "TheMovieDb"), 0);
        assert_eq!(
            super::provider_rank(&configured, super::LOCAL_SIMILARITY_PROVIDER),
            usize::MAX
        );
        // No configuration at all leaves everything first.
        assert_eq!(
            super::provider_rank(&[], super::LOCAL_SIMILARITY_PROVIDER),
            0
        );
    }

    #[tokio::test]
    async fn unticking_the_local_box_lets_the_remote_provider_run() {
        // The admin listed only TheMovieDb, i.e. unticked "Local Genre/Tag".
        // The local scorer must not run first and eat the whole limit.
        let db = test_db().await;
        let library = Uuid::from_u128(0x9ab_ce2);
        let seed = Uuid::from_u128(0x9ab_ce3);
        let local_match = Uuid::from_u128(0x9ab_ce4);
        let remote_match = Uuid::from_u128(0x9ab_ce5);
        seed_movie_in(&db, seed, "Alien", "SciFi", Some(library)).await;
        seed_movie_in(&db, local_match, "Aliens", "SciFi", Some(library)).await;
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
        let names: Vec<_> = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), Some(1))
            .await
            .expect("similar")
            .iter()
            .filter_map(|r| r.name.clone())
            .collect();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(names, ["Solaris"]);
    }

    #[tokio::test]
    async fn recommendations_never_reach_a_remote_provider() {
        // C# builds recommendation categories from the local batch provider
        // alone; fanning out to TMDB once per baseline would be a new network
        // cost on an endpoint that has none today.
        let db = test_db().await;
        let library = Uuid::from_u128(0x9ab_ce6);
        let user = Uuid::from_u128(0x9ab_ce7);
        let seed = Uuid::from_u128(0x9ab_ce8);
        let other = Uuid::from_u128(0x9ab_ce9);
        seed_movie_in(&db, seed, "Alien", "SciFi", Some(library)).await;
        seed_movie_in(&db, other, "Aliens", "SciFi", Some(library)).await;
        seed_user(&db, user).await;
        seed_user_data(&db, user, seed, true, Some(chrono::Utc::now())).await;

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
                library_id: library,
                providers: vec!["TheMovieDb".to_owned()],
            }),
        );
        mgr.get_movie_recommendations(Some(user), Uuid::nil(), 5, 5, &DtoOptions::default())
            .await
            .expect("recommendations");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no remote provider may be consulted for recommendations"
        );
    }

    #[tokio::test]
    async fn the_local_scorer_still_wins_when_it_is_ranked_first() {
        // The mirror of the test above: same data, opposite order.
        let db = test_db().await;
        let library = Uuid::from_u128(0x9ab_ce1);
        let seed = Uuid::from_u128(0x951);
        let local_match = Uuid::from_u128(0x952);
        let remote_match = Uuid::from_u128(0x953);
        seed_movie_in(&db, seed, "Alien", "SciFi", Some(library)).await;
        seed_movie_in(&db, local_match, "Aliens", "SciFi", Some(library)).await;
        seed_movie_in(&db, remote_match, "Solaris", "Drama", Some(library)).await;
        FerrofinItemPersistenceService::new(db.clone())
            .save_provider_id(remote_match, "Tmdb", "348")
            .await
            .expect("save id");

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
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })],
            Arc::new(FakeFolders {
                library_id: library,
                providers: vec!["Local Genre/Tag".to_owned(), "TheMovieDb".to_owned()],
            }),
        );
        let names: Vec<_> = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), Some(1))
            .await
            .expect("similar")
            .iter()
            .filter_map(|r| r.name.clone())
            .collect();
        assert_eq!(names, ["Aliens"]);
    }

    #[tokio::test]
    async fn a_reference_that_matches_no_library_item_is_dropped() {
        let db = test_db().await;
        let library = Uuid::from_u128(0x9ab_cdf);
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
        grant_all_folders(&db, &user).await;
        let played = Uuid::from_u128(0x312);
        let similar = Uuid::from_u128(0x313);
        seed_movie(&db, played, "Played", "SciFi|Horror").await;
        seed_movie(&db, similar, "Similar", "SciFi").await;
        seed_user_data(&db, user_id, played, true, None).await;
        // The recently-played lookup names no scope, so it is confined to the
        // user's libraries (C# `AddUserToQuery`).
        crate::test_support::seed_library_over(&db, &[played, similar]).await;

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

    #[tokio::test]
    async fn a_played_movie_is_not_suggested_to_the_user_who_played_it() {
        // C# `MovieSimilarItemsProvider` phase 2: `IsPlayed = false`.
        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(0x401)).await;
        grant_all_folders(&db, &user).await;
        let user_id = Uuid::parse_str(&user.id).expect("user id");
        let seed = Uuid::from_u128(0x402);
        let unplayed = Uuid::from_u128(0x403);
        let played = Uuid::from_u128(0x404);
        seed_movie(&db, seed, "Seed", "SciFi").await;
        seed_movie(&db, unplayed, "Unplayed", "SciFi").await;
        seed_movie(&db, played, "Played", "SciFi").await;
        seed_user_data(&db, user_id, played, true, None).await;
        let mgr = manager(&db);

        let for_user = mgr
            .get_similar_items(seed, &[], Some(user_id), &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(names(&for_user), vec!["Unplayed".to_owned()]);

        // Without a user there is no watch state to filter on (C# skips the
        // predicate when `User` is null). Both share the seed's one genre, so
        // the similarity scores tie and the sort name breaks it — alphabetical,
        // as upstream, where `BaseItem.SortName` is never null. This assertion
        // read `["Unplayed", "Played"]` while the column was NULL for both and
        // the tie fell through to insertion order.
        let anonymous = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(
            names(&anonymous),
            vec!["Played".to_owned(), "Unplayed".to_owned()]
        );
    }

    #[tokio::test]
    async fn a_library_the_user_cannot_access_contributes_nothing() {
        // C# `ConfigureUserAccess`: the query is scoped to the user's
        // `TopParentIds`. The inaccessible row is the best match (shared
        // director, +50) and must vanish; the accessible rows keep their
        // score order (director before genre).
        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(0x411)).await;
        let user_id = Uuid::parse_str(&user.id).expect("user id");
        let mine = Uuid::from_u128(0x4A1);
        let theirs = Uuid::from_u128(0x4A2);
        restrict_to_folders(&db, &user, &[mine]).await;
        let seed = Uuid::from_u128(0x412);
        let by_director = Uuid::from_u128(0x413);
        let by_genre = Uuid::from_u128(0x414);
        let hidden = Uuid::from_u128(0x415);
        seed_movie_in(&db, seed, "Seed", "SciFi", Some(mine)).await;
        seed_movie_in(&db, by_director, "SharesDirector", "Drama", Some(mine)).await;
        seed_movie_in(&db, by_genre, "SharesGenre", "SciFi", Some(mine)).await;
        seed_movie_in(&db, hidden, "HiddenLibrary", "SciFi", Some(theirs)).await;
        credit_person(&db, seed, "Chris Director", "Director").await;
        credit_person(&db, by_director, "Chris Director", "Director").await;
        credit_person(&db, hidden, "Chris Director", "Director").await;
        let mgr = manager(&db);

        let for_user = mgr
            .get_similar_items(seed, &[], Some(user_id), &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(
            names(&for_user),
            vec!["SharesDirector".to_owned(), "SharesGenre".to_owned()]
        );

        // An admin (every folder enabled) sees the hidden library's row too,
        // still in score order.
        let admin = seed_named_user(&db, Uuid::from_u128(0x416), "admin").await;
        grant_all_folders(&db, &admin).await;
        let for_admin = mgr
            .get_similar_items(
                seed,
                &[],
                Uuid::parse_str(&admin.id).ok(),
                &DtoOptions::default(),
                None,
            )
            .await
            .expect("similar");
        assert_eq!(
            names(&for_admin),
            vec![
                "HiddenLibrary".to_owned(),
                "SharesDirector".to_owned(),
                "SharesGenre".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn a_blocked_media_folder_hides_its_rows() {
        // `Folder.IsVisible`: a non-empty `BlockedMediaFolders` blocks exactly
        // those folders, whatever `EnableAllFolders` says.
        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(0x421)).await;
        grant_all_folders(&db, &user).await;
        let open = Uuid::from_u128(0x4B1);
        let blocked = Uuid::from_u128(0x4B2);
        for folder in [open, blocked] {
            crate::test_support::seed_item(&db, folder, BaseItemKind::CollectionFolder).await;
        }
        crate::user_entity_ext::set_preference(
            db.writer(),
            &user.id,
            ferrofin_db::enums::PreferenceKind::BlockedMediaFolders,
            &[blocked.to_string()],
        )
        .await
        .expect("block folder");
        let seed = Uuid::from_u128(0x422);
        seed_movie_in(&db, seed, "Seed", "SciFi", Some(open)).await;
        seed_movie_in(&db, Uuid::from_u128(0x423), "Visible", "SciFi", Some(open)).await;
        seed_movie_in(
            &db,
            Uuid::from_u128(0x424),
            "Blocked",
            "SciFi",
            Some(blocked),
        )
        .await;

        let rows = manager(&db)
            .get_similar_items(
                seed,
                &[],
                Uuid::parse_str(&user.id).ok(),
                &DtoOptions::default(),
                None,
            )
            .await
            .expect("similar");
        assert_eq!(names(&rows), vec!["Visible".to_owned()]);
    }

    #[tokio::test]
    async fn the_access_filter_does_not_starve_the_page() {
        // The score pass over-samples (C# `limit * 3`) so rows the filter
        // drops do not leave the page short.
        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(0x431)).await;
        grant_all_folders(&db, &user).await;
        let user_id = Uuid::parse_str(&user.id).expect("user id");
        let seed = Uuid::from_u128(0x432);
        seed_movie(&db, seed, "Seed", "SciFi").await;
        // Four played rows outrank (by sort name) the two unplayed ones.
        for (id, name) in [(0x440, "A"), (0x441, "B"), (0x442, "C"), (0x443, "D")] {
            let id = Uuid::from_u128(id);
            seed_movie(&db, id, name, "SciFi").await;
            seed_user_data(&db, user_id, id, true, None).await;
        }
        seed_movie(&db, Uuid::from_u128(0x450), "Y", "SciFi").await;
        seed_movie(&db, Uuid::from_u128(0x451), "Z", "SciFi").await;

        let rows = manager(&db)
            .get_similar_items(seed, &[], Some(user_id), &DtoOptions::default(), Some(2))
            .await
            .expect("similar");
        assert_eq!(names(&rows), vec!["Y".to_owned(), "Z".to_owned()]);
    }

    #[tokio::test]
    async fn an_episode_or_by_name_seed_short_circuits_but_an_artist_does_not() {
        // C# `LibraryController.GetSimilarItems`: `item is Episode ||
        // (item is IItemByName && item is not MusicArtist)` ⇒ empty.
        let db = test_db().await;
        let episode = Uuid::from_u128(0x501);
        let genre = Uuid::from_u128(0x502);
        seed_kind(&db, episode, BaseItemKind::Episode, "Pilot", "Drama").await;
        seed_kind(
            &db,
            Uuid::from_u128(0x503),
            BaseItemKind::Episode,
            "Ep 2",
            "Drama",
        )
        .await;
        seed_kind(&db, genre, BaseItemKind::Genre, "Drama", "Drama").await;
        seed_kind(
            &db,
            Uuid::from_u128(0x504),
            BaseItemKind::Genre,
            "Dramedy",
            "Drama",
        )
        .await;
        let artist = Uuid::from_u128(0x505);
        seed_kind(&db, artist, BaseItemKind::MusicArtist, "The Band", "Rock").await;
        seed_kind(
            &db,
            Uuid::from_u128(0x506),
            BaseItemKind::MusicArtist,
            "The Other Band",
            "Rock",
        )
        .await;
        let mgr = manager(&db);

        for seed in [episode, genre] {
            let rows = mgr
                .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
                .await
                .expect("similar");
            assert!(
                rows.is_empty(),
                "{seed} must short-circuit; got {:?}",
                names(&rows)
            );
        }
        let rows = mgr
            .get_similar_items(artist, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(names(&rows), vec!["The Other Band".to_owned()]);
    }

    #[tokio::test]
    async fn a_kind_without_a_local_provider_scores_nothing() {
        // C# registers no `ILocalSimilarItemsProvider` for a box set, so the
        // request proceeds past the controller guard and finds no provider.
        let db = test_db().await;
        let seed = Uuid::from_u128(0x511);
        seed_kind(&db, seed, BaseItemKind::BoxSet, "Alien Collection", "SciFi").await;
        seed_kind(
            &db,
            Uuid::from_u128(0x512),
            BaseItemKind::BoxSet,
            "Predator Collection",
            "SciFi",
        )
        .await;
        let rows = manager(&db)
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert!(rows.is_empty(), "got {:?}", names(&rows));
    }

    #[tokio::test]
    async fn exclude_artist_ids_drops_that_artists_albums_not_the_artist_row() {
        // `ExcludeArtistIds` (C# `WhereReferencedItemMultipleTypes(Artist |
        // AlbumArtist, ids, invert: true)`) removes candidates credited to the
        // artist; it is not an item-id exclusion.
        let db = test_db().await;
        let seed = Uuid::from_u128(0x601);
        let by_band = Uuid::from_u128(0x602);
        let by_other = Uuid::from_u128(0x603);
        let band = Uuid::from_u128(0x604);
        seed_kind(&db, seed, BaseItemKind::MusicAlbum, "Seed Album", "Rock").await;
        seed_kind(&db, by_band, BaseItemKind::MusicAlbum, "Band Album", "Rock").await;
        seed_kind(
            &db,
            by_other,
            BaseItemKind::MusicAlbum,
            "Other Album",
            "Rock",
        )
        .await;
        seed_kind(&db, band, BaseItemKind::MusicArtist, "The Band", "Rock").await;
        seed_album_artist(&db, by_band, "The Band").await;
        seed_album_artist(&db, by_other, "Someone Else").await;
        let mgr = manager(&db);

        let all = mgr
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(
            names(&all),
            vec!["Band Album".to_owned(), "Other Album".to_owned()]
        );
        let without_band = mgr
            .get_similar_items(seed, &[band], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(names(&without_band), vec!["Other Album".to_owned()]);
    }

    #[tokio::test]
    async fn exclude_artist_ids_is_ignored_for_movies() {
        // Only the music providers honour `ExcludeArtistIds`; a movie request
        // carrying one (clients send it on every alias) is unaffected.
        let db = test_db().await;
        let seed = Uuid::from_u128(0x611);
        let other = Uuid::from_u128(0x612);
        seed_movie(&db, seed, "Seed", "SciFi").await;
        seed_movie(&db, other, "Other", "SciFi").await;
        let rows = manager(&db)
            .get_similar_items(seed, &[other], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(names(&rows), vec!["Other".to_owned()]);
    }

    #[tokio::test]
    async fn external_content_widens_movie_suggestions_when_enabled() {
        // `EnableExternalContentInSuggestions` folds trailers and Live TV
        // programs into the movie provider's kinds (and nothing else — a
        // series sharing the genre never qualifies).
        let db = test_db().await;
        let seed = Uuid::from_u128(0x621);
        seed_movie(&db, seed, "Seed", "SciFi").await;
        seed_kind(
            &db,
            Uuid::from_u128(0x622),
            BaseItemKind::Trailer,
            "Trailer",
            "SciFi",
        )
        .await;
        seed_kind(
            &db,
            Uuid::from_u128(0x623),
            BaseItemKind::Series,
            "Series",
            "SciFi",
        )
        .await;

        let on = manager(&db).with_configuration(Arc::new(FakeConfig { external: true }));
        let rows = on
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert_eq!(names(&rows), vec!["Trailer".to_owned()]);

        let off = manager(&db).with_configuration(Arc::new(FakeConfig { external: false }));
        let rows = off
            .get_similar_items(seed, &[], None, &DtoOptions::default(), None)
            .await
            .expect("similar");
        assert!(rows.is_empty(), "got {:?}", names(&rows));
    }

    #[tokio::test]
    async fn recommendations_skip_the_users_played_movies() {
        // The batch provider is handed the user, so a category never
        // recommends a film the user already watched.
        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(0x631)).await;
        grant_all_folders(&db, &user).await;
        let user_id = Uuid::parse_str(&user.id).expect("user id");
        let played = Uuid::from_u128(0x632);
        let also_played = Uuid::from_u128(0x633);
        let fresh = Uuid::from_u128(0x634);
        seed_movie(&db, played, "Played", "SciFi").await;
        seed_movie(&db, also_played, "AlsoPlayed", "SciFi").await;
        seed_movie(&db, fresh, "Fresh", "SciFi").await;
        seed_user_data(&db, user_id, played, true, None).await;
        seed_user_data(&db, user_id, also_played, true, None).await;

        let recs = manager(&db)
            .get_movie_recommendations(Some(user_id), Uuid::nil(), 6, 5, &DtoOptions::default())
            .await
            .expect("recommendations");
        for category in recs
            .iter()
            .filter(|r| r.recommendation_type == RecommendationType::SimilarToRecentlyPlayed)
        {
            assert_eq!(
                names(&category.items),
                vec!["Fresh".to_owned()],
                "category {:?}",
                category.baseline_item_name
            );
        }
    }
}
