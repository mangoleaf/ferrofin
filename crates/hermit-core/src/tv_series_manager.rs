//! [`HermitTvSeriesManager`] — the concrete [`TvSeriesManager`] (Next Up queue).
//!
//! Port of `Emby.Server.Implementations.TV.TVSeriesManager`. This is the *real*
//! (non-stub) manager of Wave 8's unit 8: it computes a user's "Next Up" episode
//! list by delegating the per-series next-up algorithm to the unit-2
//! [`NextUpService`](hermit_traits::persistence::NextUpService) and then
//! projecting the resulting episode rows to
//! [`BaseItemDto`](hermit_model::dto::BaseItemDto) through the injected
//! [`DtoService`](hermit_traits::dto::DtoService), paginating with the same
//! `start_index`/`limit`/`enable_total_record_count` semantics as C# `GetResult`.
//!
//! Port rules applied:
//! - The two C# `GetNextUp` overloads collapse to the single trait method
//!   [`TvSeriesManager::get_next_up`]; the explicit `BaseItem[] parentsFolders`
//!   overload is folded into the query's [`NextUpQuery::parent_id`].
//! - The C# `query.User` domain object is resolved from
//!   [`NextUpQuery::user_id`] through the injected
//!   [`UserManager`](hermit_traits::library::UserManager); the derived
//!   [`UserEntity`](hermit_db::entities::users::UserEntity) then rides the
//!   [`InternalItemsQuery`] the way C# builds `new InternalItemsQuery(user)`.
//! - The un-ported `Series`/`Episode` domain tree is not reconstructed: the
//!   presentation key for a `series_id` is read straight off the persisted row
//!   (`PresentationUniqueKey ?? Id`, matching
//!   `Series.GetPresentationUniqueKey()`), and the last-played-version
//!   preference (`GetPreferredVersion`/`GetMostRecentlyPlayedVersion`) — which
//!   needs the `Video`'s media-source list — is a documented deferral. The
//!   next-up episodes come back already ordered by the service, so the picked
//!   episode per series is taken directly.
//! - `DisplaySpecialsWithinSeasons` is read from the injected
//!   [`ServerConfigurationManager`](hermit_traits::configuration::ServerConfigurationManager),
//!   as in C#.
//! - Synchronous C# methods become `async fn -> Result<_, ServiceError>` (the
//!   impl paginates the database via its injected repositories).

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::dto::BaseItemDto;
use hermit_model::querying::QueryResult;

use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, UserManager};
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use hermit_traits::persistence::NextUpService;
use hermit_traits::tv::{NextUpQuery, TvSeriesManager};

/// The concrete TV-series (Next Up) manager.
///
/// Holds its collaborating managers behind `Arc<dyn _>` so they can be injected
/// at the Wave 8 composition root; this crate depends only on the traits.
#[derive(Clone)]
pub struct HermitTvSeriesManager {
    user_manager: Arc<dyn UserManager>,
    library_manager: Arc<dyn LibraryManager>,
    next_up_service: Arc<dyn NextUpService>,
    dto_service: Arc<dyn DtoService>,
    configuration_manager: Arc<dyn ServerConfigurationManager>,
}

impl std::fmt::Debug for HermitTvSeriesManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitTvSeriesManager")
            .finish_non_exhaustive()
    }
}

impl HermitTvSeriesManager {
    /// Creates a TV-series manager from its injected collaborators.
    #[must_use]
    pub fn new(
        user_manager: Arc<dyn UserManager>,
        library_manager: Arc<dyn LibraryManager>,
        next_up_service: Arc<dyn NextUpService>,
        dto_service: Arc<dyn DtoService>,
        configuration_manager: Arc<dyn ServerConfigurationManager>,
    ) -> Self {
        Self {
            user_manager,
            library_manager,
            next_up_service,
            dto_service,
            configuration_manager,
        }
    }

    /// The presentation unique key of a series row: its explicit
    /// `PresentationUniqueKey` when set, else its id (mirrors
    /// `Series.GetPresentationUniqueKey()`).
    fn series_presentation_key(series: &BaseItemEntity) -> String {
        series
            .presentation_unique_key
            .clone()
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| series.id.clone())
    }

    /// Resolves the top-parent library folders the next-up scan is scoped to.
    ///
    /// When `parent_id` is set, that single parent is the scope (the C#
    /// `parents = [parent]` branch). Otherwise the scope is every top-level
    /// library folder (the C# `GetUserRootFolder().GetChildren(...).Where(Folder)`
    /// branch); the per-user `LatestItemExcludes` preference filter is a
    /// documented deferral (it needs the un-ported preference tree).
    async fn resolve_top_parents(
        &self,
        parent_id: Option<uuid::Uuid>,
    ) -> Result<Vec<uuid::Uuid>, ServiceError> {
        if let Some(parent) = parent_id {
            // Only scope to it if it actually exists (C# `parent is not null`).
            if self.library_manager.get_item_by_id(parent).await?.is_some() {
                return Ok(vec![parent]);
            }
            return Ok(Vec::new());
        }

        let mut query = InternalItemsQuery {
            is_folder: Some(true),
            ..InternalItemsQuery::default()
        };
        query.parent_id = uuid::Uuid::nil();
        let folders = self.library_manager.get_item_list(&query).await?;
        Ok(folders
            .into_iter()
            .filter_map(|f| uuid::Uuid::parse_str(&f.id).ok())
            .collect())
    }

    /// Runs the batched next-up algorithm for a set of series keys and returns
    /// the picked (last-watched-date, episode-row) list, newest first.
    ///
    /// Port of `GetNextUpBatched`: it asks the [`NextUpService`] for each
    /// series' batch result and takes the service-selected `next_up` (and, when
    /// rewatching is enabled, `next_played_for_rewatching`) episode. The version
    /// preference re-selection is deferred (see the module docs).
    async fn next_up_batched(
        &self,
        request: &NextUpQuery,
        user: &UserEntity,
        series_keys: &[String],
        dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if series_keys.is_empty() {
            return Ok(Vec::new());
        }

        let include_specials = self
            .configuration_manager
            .configuration()
            .await?
            .display_specials_within_seasons;
        let include_rewatching = request.enable_rewatching;

        let query = InternalItemsQuery {
            user: Some(user.clone()),
            dto_options: dto_options.clone(),
            ..InternalItemsQuery::default()
        };

        let batch = self
            .next_up_service
            .get_next_up_episodes_batch(&query, series_keys, include_specials, include_rewatching)
            .await?;

        // Preserve the series-key ordering the caller supplied (which is
        // last-played-date descending from `get_next_up_series_keys`), and within
        // it favour the fresh next-up over the rewatching pick, exactly as the
        // C# `nextUpList` is appended and then order-preserved by watch date.
        let mut episodes: Vec<BaseItemEntity> = Vec::new();
        for key in series_keys {
            let Some(result) = batch.get(key) else {
                continue;
            };
            if let Some(next) = result.next_up.clone() {
                episodes.push(next);
            }
            if include_rewatching
                && let Some(next_played) = result.next_played_for_rewatching.clone()
            {
                episodes.push(next_played);
            }
        }

        Ok(episodes)
    }

    /// Paginates the selected episode rows into a DTO query result.
    ///
    /// Port of the static `GetResult`: total count is computed only when
    /// `enable_total_record_count`, then `start_index`/`limit` are applied.
    async fn to_result(
        &self,
        episodes: Vec<BaseItemEntity>,
        request: &NextUpQuery,
        user: &UserEntity,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        let total_count = if request.enable_total_record_count {
            i32::try_from(episodes.len()).unwrap_or(i32::MAX)
        } else {
            0
        };

        let start = request
            .start_index
            .and_then(|s| usize::try_from(s).ok())
            .unwrap_or(0);
        let mut page: Vec<BaseItemEntity> = episodes.into_iter().skip(start).collect();
        if let Some(limit) = request.limit
            && limit > 0
        {
            page.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        let dtos = self
            .dto_service
            .get_base_item_dtos(&page, options, Some(user), None, true)
            .await?;

        Ok(QueryResult::new(
            request.start_index,
            request.enable_total_record_count.then_some(total_count),
            dtos,
        ))
    }
}

#[async_trait]
impl TvSeriesManager for HermitTvSeriesManager {
    async fn get_next_up(
        &self,
        query: &NextUpQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        let user = self
            .user_manager
            .get_user_by_id(query.user_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("user {}", query.user_id)))?;

        // Single-series path: resolve its presentation key and batch just it.
        // (When the series id does not resolve, C# leaves presentationUniqueKey
        // null and falls through to the parent scan below.)
        if let Some(series_id) = query.series_id
            && let Some(series) = self.library_manager.get_item_by_id(series_id).await?
        {
            let key = Self::series_presentation_key(&series);
            let episodes = self.next_up_batched(query, &user, &[key], options).await?;
            return self.to_result(episodes, query, &user, options).await;
        }

        // Library-wide path: find eligible series keys under the scoped parents.
        let top_parents = self.resolve_top_parents(query.parent_id).await?;
        if top_parents.is_empty() {
            return Ok(QueryResult::new(
                query.start_index,
                query.enable_total_record_count.then_some(0),
                Vec::new(),
            ));
        }

        let cutoff = query
            .next_up_date_cutoff
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);

        let keys_query = InternalItemsQuery {
            user: Some(user.clone()),
            limit: query.limit.map(|l| l + 10),
            top_parent_ids: top_parents,
            ..InternalItemsQuery::default()
        };
        let series_keys = self
            .next_up_service
            .get_next_up_series_keys(&keys_query, cutoff)
            .await?;

        let episodes = self
            .next_up_batched(query, &user, &series_keys, options)
            .await?;
        self.to_result(episodes, query, &user, options).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use hermit_db::entities::base_items::BaseItemEntity;
    use hermit_db::entities::users::UserEntity;
    use hermit_db::store::guid_to_db;
    use hermit_model::dto::BaseItemDto;
    use uuid::Uuid;

    use hermit_traits::configuration::ServerConfigurationManager;
    use hermit_traits::dto::DtoService;
    use hermit_traits::error::ServiceError;
    use hermit_traits::library::{LibraryManager, UserManager};
    use hermit_traits::options::{DtoOptions, InternalItemsQuery};
    use hermit_traits::persistence::{NextUpEpisodeBatchResult, NextUpService};
    use hermit_traits::tv::{NextUpQuery, TvSeriesManager};

    use crate::test_support::{seed_episode, seed_user, test_db};

    use super::HermitTvSeriesManager;

    // ── Minimal fakes for the injected collaborators ──
    //
    // Real `BaseItemEntity`/`UserEntity` rows have no `Default`, so the fixtures
    // seed a throwaway in-memory `hermit-db` and read the rows back rather than
    // hand-constructing the ~60-/34-column structs.

    struct FakeUserManager {
        user: UserEntity,
    }
    #[async_trait]
    impl UserManager for FakeUserManager {
        async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
            Ok(vec![self.user.clone()])
        }
        async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(vec![Uuid::parse_str(&self.user.id).unwrap()])
        }
        async fn initialize(&self) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
            Ok((self.user.id == guid_to_db(id)).then(|| self.user.clone()))
        }
        async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
            Ok(Some(self.user.clone()))
        }
        async fn get_user_by_name(&self, _name: &str) -> Result<Option<UserEntity>, ServiceError> {
            Ok(Some(self.user.clone()))
        }
        async fn rename_user(
            &self,
            _id: Uuid,
            _old_name: &str,
            _new_name: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn create_user(&self, _name: &str) -> Result<UserEntity, ServiceError> {
            Ok(self.user.clone())
        }
        async fn delete_user(&self, _user_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn reset_password(&self, _user_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn change_password(
            &self,
            _user_id: Uuid,
            _new_password: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn authenticate_user(
            &self,
            _username: &str,
            _password: &str,
            _remote_endpoint: &str,
            _is_user_session: bool,
        ) -> Result<Option<UserEntity>, ServiceError> {
            Ok(Some(self.user.clone()))
        }
        async fn get_authentication_providers(
            &self,
        ) -> Result<Vec<hermit_model::dto::NameIdPair>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_password_reset_providers(
            &self,
        ) -> Result<Vec<hermit_model::dto::NameIdPair>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_user_dto(
            &self,
            user: &UserEntity,
            server_id: Option<String>,
        ) -> Result<hermit_model::dto::UserDto, ServiceError> {
            Ok(hermit_model::dto::UserDto {
                id: Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil()),
                name: Some(user.username.clone()),
                server_id,
                ..hermit_model::dto::UserDto::default()
            })
        }
        async fn update_configuration(
            &self,
            _user_id: Uuid,
            _config: &hermit_model::configuration::UserConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn update_policy(
            &self,
            _user_id: Uuid,
            _policy: &hermit_model::users::UserPolicy,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    struct FakeLibraryManager {
        items: HashMap<Uuid, BaseItemEntity>,
        top_folders: Vec<BaseItemEntity>,
    }
    #[async_trait]
    impl LibraryManager for FakeLibraryManager {
        async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
            Ok(self.items.get(&id).cloned())
        }
        async fn get_item_images(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<hermit_traits::options::ItemImageInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn query_items(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<hermit_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
            Ok(hermit_model::querying::QueryResult::new(
                None,
                None,
                Vec::new(),
            ))
        }
        async fn get_item_ids(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_item_list(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(self.top_folders.clone())
        }
        async fn get_latest_item_list(
            &self,
            _query: &InternalItemsQuery,
            _collection_type: hermit_model::data::CollectionType,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(Vec::new())
        }
        async fn create_items(
            &self,
            _items: &[BaseItemEntity],
            _parent_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn update_items(
            &self,
            _items: &[BaseItemEntity],
            _parent_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_item(
            &self,
            _id: Uuid,
            _options: &hermit_traits::options::DeleteOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_people(
            &self,
            _query: &hermit_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<hermit_db::entities::base_items::PeopleEntity>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_people_names(
            &self,
            _query: &hermit_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_count(&self, _query: &InternalItemsQuery) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_item_counts(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
            Ok(hermit_model::dto::ItemCounts::default())
        }
        async fn get_genres(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::new(
                None,
                None,
                Vec::new(),
            ))
        }
        async fn get_studios(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::new(
                None,
                None,
                Vec::new(),
            ))
        }
        async fn get_artists(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::new(
                None,
                None,
                Vec::new(),
            ))
        }
        async fn get_music_genres(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::new(
                None,
                None,
                Vec::new(),
            ))
        }
        async fn get_album_artists(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::new(
                None,
                None,
                Vec::new(),
            ))
        }
        async fn get_query_filters_legacy(
            &self,
            _query: &InternalItemsQuery,
        ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
            Ok(hermit_model::querying::QueryFiltersLegacy::default())
        }
        async fn get_media_stream_languages(
            &self,
            _stream_type: hermit_model::entities::MediaStreamType,
            _query: &InternalItemsQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(Vec::new())
        }
        async fn queue_library_scan(&self) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    struct FakeNextUpService {
        keys: Vec<String>,
        batch: HashMap<String, NextUpEpisodeBatchResult>,
    }
    #[async_trait]
    impl NextUpService for FakeNextUpService {
        async fn get_next_up_series_keys(
            &self,
            _filter: &InternalItemsQuery,
            _date_cutoff: chrono::DateTime<chrono::Utc>,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(self.keys.clone())
        }
        async fn get_next_up_episodes_batch(
            &self,
            _filter: &InternalItemsQuery,
            _series_keys: &[String],
            _include_specials: bool,
            _include_watched_for_rewatching: bool,
        ) -> Result<HashMap<String, NextUpEpisodeBatchResult>, ServiceError> {
            Ok(self.batch.clone())
        }
    }

    struct FakeDtoService;
    #[async_trait]
    impl DtoService for FakeDtoService {
        async fn get_primary_image_aspect_ratio(
            &self,
            _item_id: Uuid,
        ) -> Result<Option<f64>, ServiceError> {
            Ok(None)
        }
        async fn get_base_item_dto(
            &self,
            item: &BaseItemEntity,
            _options: &DtoOptions,
            _user: Option<&UserEntity>,
            _owner_id: Option<Uuid>,
        ) -> Result<BaseItemDto, ServiceError> {
            Ok(BaseItemDto {
                id: Uuid::parse_str(&item.id).unwrap_or_default(),
                ..BaseItemDto::default()
            })
        }
        async fn get_base_item_dtos(
            &self,
            items: &[BaseItemEntity],
            _options: &DtoOptions,
            _user: Option<&UserEntity>,
            _owner_id: Option<Uuid>,
            _skip_visibility_check: bool,
        ) -> Result<Vec<BaseItemDto>, ServiceError> {
            Ok(items
                .iter()
                .map(|i| BaseItemDto {
                    id: Uuid::parse_str(&i.id).unwrap_or_default(),
                    ..BaseItemDto::default()
                })
                .collect())
        }
        async fn get_item_by_name_dto(
            &self,
            _item: &BaseItemEntity,
            _options: &DtoOptions,
            _tagged_item_ids: Option<&[Uuid]>,
            _user: Option<&UserEntity>,
        ) -> Result<BaseItemDto, ServiceError> {
            Ok(BaseItemDto::default())
        }
    }

    struct FakeConfigManager {
        include_specials: bool,
    }
    #[async_trait]
    impl ServerConfigurationManager for FakeConfigManager {
        fn application_paths(&self) -> Arc<dyn hermit_traits::system::ServerApplicationPaths> {
            unreachable!("application_paths not used in next-up tests")
        }
        async fn configuration(
            &self,
        ) -> Result<hermit_model::configuration::ServerConfiguration, ServiceError> {
            let mut c = crate::configuration_manager::default_server_configuration();
            c.display_specials_within_seasons = self.include_specials;
            Ok(c)
        }
        async fn update_configuration(
            &self,
            _config: &hermit_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<hermit_model::branding::BrandingOptions, ServiceError> {
            Ok(hermit_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _branding: &hermit_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn manager(
        user: UserEntity,
        library: FakeLibraryManager,
        next_up: FakeNextUpService,
        include_specials: bool,
    ) -> HermitTvSeriesManager {
        HermitTvSeriesManager::new(
            Arc::new(FakeUserManager { user }),
            Arc::new(library),
            Arc::new(next_up),
            Arc::new(FakeDtoService),
            Arc::new(FakeConfigManager { include_specials }),
        )
    }

    /// Seeds a real `Users` row and reads it back as a `UserEntity`.
    async fn seeded_user(db: &hermit_db::Database, id: Uuid) -> UserEntity {
        seed_user(db, id).await
    }

    /// Seeds a real `Episode` `BaseItems` row and reads it back as an entity.
    async fn seeded_episode(db: &hermit_db::Database, id: Uuid) -> BaseItemEntity {
        seed_episode(db, id, "series-key", 1, 1, false, None).await;
        sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("fetch episode row")
    }

    /// Seeds a real top-level folder `BaseItems` row and reads it back.
    async fn seeded_folder(db: &hermit_db::Database, id: Uuid) -> BaseItemEntity {
        crate::test_support::seed_item(db, id, hermit_model::data::BaseItemKind::Folder).await;
        sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("fetch folder row")
    }

    #[tokio::test]
    async fn library_wide_next_up_projects_picked_episodes() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let ep = Uuid::new_v4();
        let folder_id = Uuid::new_v4();

        let user = seeded_user(&db, user_id).await;
        let folder = seeded_folder(&db, folder_id).await;
        let episode = seeded_episode(&db, ep).await;

        let mut batch = HashMap::new();
        batch.insert(
            "series-a".to_owned(),
            NextUpEpisodeBatchResult {
                next_up: Some(episode),
                ..NextUpEpisodeBatchResult::default()
            },
        );

        let mgr = manager(
            user,
            FakeLibraryManager {
                items: HashMap::new(),
                top_folders: vec![folder],
            },
            FakeNextUpService {
                keys: vec!["series-a".to_owned()],
                batch,
            },
            false,
        );

        let query = NextUpQuery {
            user_id,
            enable_total_record_count: true,
            ..NextUpQuery::default()
        };
        let result = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect("next up");

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].id, ep);
        assert_eq!(result.total_record_count, 1);
    }

    #[tokio::test]
    async fn missing_user_is_not_found() {
        let db = test_db().await;
        // Seed a user, but query a *different* id → lookup misses.
        let user = seeded_user(&db, Uuid::new_v4()).await;
        let mgr = manager(
            user,
            FakeLibraryManager {
                items: HashMap::new(),
                top_folders: Vec::new(),
            },
            FakeNextUpService {
                keys: Vec::new(),
                batch: HashMap::new(),
            },
            false,
        );
        let query = NextUpQuery {
            user_id: Uuid::new_v4(),
            ..NextUpQuery::default()
        };
        let err = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect_err("missing user");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn limit_and_start_index_paginate() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let folder_id = Uuid::new_v4();
        let user = seeded_user(&db, user_id).await;
        let folder = seeded_folder(&db, folder_id).await;

        let mut batch = HashMap::new();
        for i in 0..3 {
            let ep = Uuid::new_v4();
            let episode = seeded_episode(&db, ep).await;
            batch.insert(
                format!("series-{i}"),
                NextUpEpisodeBatchResult {
                    next_up: Some(episode),
                    ..NextUpEpisodeBatchResult::default()
                },
            );
        }
        let keys: Vec<String> = (0..3).map(|i| format!("series-{i}")).collect();

        let mgr = manager(
            user,
            FakeLibraryManager {
                items: HashMap::new(),
                top_folders: vec![folder],
            },
            FakeNextUpService { keys, batch },
            false,
        );

        let query = NextUpQuery {
            user_id,
            start_index: Some(1),
            limit: Some(1),
            enable_total_record_count: true,
            ..NextUpQuery::default()
        };
        let result = mgr
            .get_next_up(&query, &DtoOptions::default())
            .await
            .expect("next up");

        // 3 total, skip 1, take 1.
        assert_eq!(result.total_record_count, 3);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.start_index, 1);
    }
}
