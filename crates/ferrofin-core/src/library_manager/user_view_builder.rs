//! `UserView.GetItemsInternal` (`UserView.cs:96-109`): a browse whose parent
//! is a user view runs through `UserViewBuilder.GetUserItems`, which picks the
//! view's builder by its `ViewType`.

use std::sync::Arc;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager as _;
use ferrofin_traits::options::InternalItemsQuery;
use uuid::Uuid;

use super::FerrofinLibraryManager;

/// `GetMovieFolders`' sub-views (`UserViewBuilder.cs:150-158`, v12.4), in list
/// order: each view type with the localization key of its name and its
/// `ForcedSortName`.
const MOVIE_SUB_VIEWS: [(CollectionType, &str, &str); 6] = [
    (CollectionType::Movieresume, "HeaderContinueWatching", "0"),
    (CollectionType::Movielatest, "Latest", "1"),
    (CollectionType::Moviemovies, "Movies", "2"),
    (CollectionType::Moviecollection, "Collections", "3"),
    (CollectionType::Moviefavorites, "Favorites", "4"),
    (CollectionType::Moviegenres, "Genres", "5"),
];

/// `GetTvView`'s sub-views (`UserViewBuilder.cs:318-327`, v12.4), as
/// [`MOVIE_SUB_VIEWS`].
const TV_SUB_VIEWS: [(CollectionType, &str, &str); 7] = [
    (CollectionType::Tvresume, "HeaderContinueWatching", "0"),
    (CollectionType::Tvnextup, "HeaderNextUp", "1"),
    (CollectionType::Tvlatest, "Latest", "2"),
    (CollectionType::Tvshowseries, "Shows", "3"),
    (CollectionType::Tvfavoriteseries, "HeaderFavoriteShows", "4"),
    (
        CollectionType::Tvfavoriteepisodes,
        "HeaderFavoriteEpisodes",
        "5",
    ),
    (CollectionType::Tvgenres, "Genres", "6"),
];

/// `GetSpecialItemsLimit()` (`UserViewBuilder.cs:130-133`, v12.4): the most a
/// latest or resume view lists, whatever the request's limit.
const SPECIAL_ITEMS_LIMIT: i32 = 50;

impl FerrofinLibraryManager {
    /// [`LibraryManager::apply_user_view_builder`](ferrofin_traits::library::LibraryManager::apply_user_view_builder).
    pub(super) async fn user_view_builder(
        &self,
        query: &mut InternalItemsQuery,
        view: &BaseItemEntity,
    ) -> Result<(Option<BaseItemKind>, bool), ServiceError> {
        // UserViewBuilder's movie/TV recursive base views call QueryRecursive;
        // folders/direct repository/subnavigation builders do not auto-collapse.
        // ViewType belongs to the view row already read for access, including
        // adopted numeric values; it is never a new client preference.
        let (view_type, parent_id) = user_view_browse_context(view);
        if user_view_uses_direct_items(view_type) {
            // These direct builders set their kinds and recursive mode
            // regardless of the request, then forward explicit collapse to
            // SQL. Their automatic global grouping policy is intentionally
            // absent.
            let kept_parent = apply_sub_view_builder(query, view_type);
            return Ok((kept_parent.then(|| kind_from(view)).flatten(), false));
        }
        if matches!(
            view_type,
            Some(CollectionType::movies | CollectionType::tvshows)
        ) {
            if !query.recursive {
                // `GetMovieFolders`/`GetTvView` list the view's sub-views
                // ([`FerrofinLibraryManager::user_view_list_result`]).
                return Ok((kind_from(view), false));
            }
            query.folder_query_recursive = true;
            if query.include_item_types.is_empty() {
                query.include_item_types = if view_type == Some(CollectionType::movies) {
                    vec![BaseItemKind::Movie]
                } else {
                    vec![
                        BaseItemKind::Series,
                        BaseItemKind::Season,
                        BaseItemKind::Episode,
                    ]
                };
            }
            if let Some(parent_id) = parent_id
                && let Some(query_parent) = self.get_item_by_id(parent_id).await?
            {
                return Ok((kind_from(&query_parent), true));
            }
            return Ok((kind_from(view), true));
        }
        // The default builder delegates to its actual display/relational parent
        // when one exists; an unparented UserView uses GetResult over media-folder
        // children and has no automatic grouping policy.
        if let Some(parent_id) = parent_id
            && let Some(query_parent) = self.get_item_by_id(parent_id).await?
        {
            let (kind, folder_policy) = (kind_from(&query_parent), !is_channel(&query_parent));
            let lists_own_children = query.user.is_none()
                && matches!(
                    kind,
                    Some(BaseItemKind::PlaylistsFolder | BaseItemKind::ManualPlaylistsFolder)
                );
            if !query.recursive || kind == Some(BaseItemKind::Season) || lists_own_children {
                // A default view delegates to that Season's GetItemsInternal even
                // for recursive requests; its aired selection needs the real row.
                // So does a playlists folder's with no user, which lists the
                // folder's own children (PlaylistsFolder.GetItemsInternal).
                query.parent_id = parent_id;
                query.user_root_children = false;
                query.parent_row = Some(Arc::new(query_parent));
            }
            return Ok((kind, folder_policy));
        }
        Ok((kind_from(view), false))
    }

    /// The `UserViewBuilder.GetUserItems` arms that list views rather than
    /// query items (v12.4): a movies or TV view browsed without `Recursive`
    /// lists its sub-views (`GetMovieFolders`, `UserViewBuilder.cs:135-161`;
    /// `GetTvView`, `:298-330`), and a movies or TV genres view, browsed
    /// either way, its genre views (`GetMovieGenres`, `:258-284`;
    /// `GetTvGenres`, `:386-412`). Each answers through `GetResult`
    /// (`:426-434`): `Filter`, then `SortAndPage`, which counts every row
    /// before it pages whatever the request says. `None` for any other query.
    ///
    /// # Errors
    ///
    /// Without the user-view manager wired, or when a sub-view cannot be
    /// read or made.
    pub(super) async fn user_view_list_result(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Option<QueryResult<BaseItemEntity>>, ServiceError> {
        if query.parent_type != Some(BaseItemKind::UserView)
            || query.parent_id.is_nil()
            || !query.item_ids.is_empty()
        {
            return Ok(None);
        }
        let Some(view) = self.parent_item(query).await? else {
            return Ok(None);
        };
        let (view_type, parent_id) = user_view_browse_context(&view);
        let sub_views = match view_type {
            Some(CollectionType::movies) if !query.recursive => MOVIE_SUB_VIEWS.as_slice(),
            Some(CollectionType::tvshows) if !query.recursive => TV_SUB_VIEWS.as_slice(),
            Some(CollectionType::Moviegenres | CollectionType::Tvgenres) => &[],
            _ => return Ok(None),
        };
        let user_views = self
            .user_views
            .as_ref()
            .ok_or_else(|| ServiceError::backend("user views are not configured"))?;
        // `UserView.GetItemsInternal`: the display parent (else the parent)
        // when it is a folder, else the view itself.
        let parent = match parent_id {
            Some(id) => self
                .get_item_by_id(id)
                .await?
                .filter(|row| row.is_folder)
                .map_or_else(|| Arc::clone(&view), Arc::new),
            None => Arc::clone(&view),
        };
        let rows = if let Some((kind, genre_view)) = match view_type {
            Some(CollectionType::Moviegenres) => {
                Some((BaseItemKind::Movie, CollectionType::Moviegenre))
            }
            Some(CollectionType::Tvgenres) => Some((BaseItemKind::Series, CollectionType::Tvgenre)),
            _ => None,
        } {
            self.genre_views(user_views, query, &parent, kind, genre_view)
                .await?
        } else {
            let mut rows = Vec::with_capacity(sub_views.len());
            for (sub_view, key, sort_name) in sub_views {
                if let Some(row) = user_views
                    .user_sub_view(&parent, *sub_view, key, sort_name)
                    .await?
                {
                    rows.push(row);
                }
            }
            rows
        };
        let rows = self.items.filter_folder_children(rows, query).await?;
        self.folder_post_filter_result(rows, query).await.map(Some)
    }

    /// `GetMovieGenres`/`GetTvGenres`' list: the genres of every `kind` item
    /// under `parent` (`QueryRecursive`, uncounted), distinct by name, each
    /// that `GetGenre` resolves listed as `GetUserViewWithName(genre_view,
    /// genre.SortName, parent)`. That sub-view is keyed by the parent and the
    /// view type alone and named the parent's id in `N` form (passed as the
    /// localization key, `UserViewBuilder.cs:884-887`), so upstream lists one
    /// view once per genre, its `ForcedSortName` the first genre's (measured
    /// on 12.4); so does this.
    async fn genre_views(
        &self,
        user_views: &crate::FerrofinUserViewManager,
        query: &InternalItemsQuery,
        parent: &BaseItemEntity,
        kind: BaseItemKind,
        genre_view: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let Ok(parent_id) = Uuid::parse_str(&parent.id) else {
            return Ok(Vec::new());
        };
        let items = self
            .items
            .get_item_list(&InternalItemsQuery {
                user: query.user.clone(),
                parent_id,
                include_item_types: vec![kind],
                recursive: true,
                enable_total_record_count: false,
                ..InternalItemsQuery::default()
            })
            .await?;
        let names = crate::music_manager::distinct_names(
            items
                .iter()
                .flat_map(crate::music_manager::row_genres)
                .collect(),
        );
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let genres: Vec<BaseItemEntity> = self
            .get_named_items(BaseItemKind::Genre, &names)
            .await?
            .into_iter()
            .flatten()
            .collect();
        let Some(first) = genres.first() else {
            return Ok(Vec::new());
        };
        // Every later call finds the view the first made and changes nothing.
        let sort_name = first
            .sort_name
            .clone()
            .or_else(|| crate::kinds::fallback_sort_name(first))
            .unwrap_or_default();
        let Some(view) = user_views
            .user_sub_view(
                parent,
                genre_view,
                &parent_id.simple().to_string(),
                &sort_name,
            )
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(vec![view; genres.len()])
    }
}

/// The query a direct builder runs (v12.4 `UserViewBuilder.cs`), whatever the
/// request asked: the kinds, recursion, filters, order and limit each forces.
/// The parent stays the view, which scopes a recursive query as upstream's
/// `query.Parent = parent` does (by the view's display parent), except for
/// `GetMovieCollections`, which clears it. The latest and resume views read a
/// list (`ConvertToResult(GetItemList)`), whose total is its length, so their
/// count is off. A genre view's `GenreIds` is the view's own id
/// (`displayParent.Id`). The other direct builders (`folders`, the genres
/// views, next up) leave the query as it came. Returns `false` when the
/// builder cleared the parent.
fn apply_sub_view_builder(
    query: &mut InternalItemsQuery,
    view_type: Option<CollectionType>,
) -> bool {
    let latest = [
        (ItemSortBy::DateCreated, SortOrder::Descending),
        (ItemSortBy::SortName, SortOrder::Descending),
    ];
    let resume = [
        (ItemSortBy::DatePlayed, SortOrder::Descending),
        (ItemSortBy::SortName, SortOrder::Descending),
    ];
    let mut kept_parent = true;
    let kind = match view_type {
        // `GetMovieMovies`, `GetTvSeries`, `GetBooks`.
        Some(CollectionType::Moviemovies) => vec![BaseItemKind::Movie],
        Some(CollectionType::Tvshowseries) => vec![BaseItemKind::Series],
        Some(CollectionType::books) => vec![BaseItemKind::Book, BaseItemKind::AudioBook],
        // `GetFavoriteMovies`, `GetFavoriteSeries`, `GetFavoriteEpisodes`.
        Some(
            view @ (CollectionType::Moviefavorites
            | CollectionType::Tvfavoriteseries
            | CollectionType::Tvfavoriteepisodes),
        ) => {
            query.is_favorite = Some(true);
            vec![match view {
                CollectionType::Moviefavorites => BaseItemKind::Movie,
                CollectionType::Tvfavoriteseries => BaseItemKind::Series,
                _ => BaseItemKind::Episode,
            }]
        }
        // `GetMovieLatest`, `GetTvLatest`; `GetMovieResume`, `GetTvResume`.
        Some(
            view @ (CollectionType::Movielatest
            | CollectionType::Tvlatest
            | CollectionType::Movieresume
            | CollectionType::Tvresume),
        ) => {
            let resumable = matches!(view, CollectionType::Movieresume | CollectionType::Tvresume);
            query.order_by = if resumable { resume } else { latest }.to_vec();
            if resumable {
                query.is_resumable = Some(true);
            }
            query.limit = Some(SPECIAL_ITEMS_LIMIT);
            query.enable_total_record_count = false;
            if view == CollectionType::Tvlatest {
                query.is_virtual_item = Some(false);
            }
            vec![match view {
                CollectionType::Movielatest | CollectionType::Movieresume => BaseItemKind::Movie,
                _ => BaseItemKind::Episode,
            }]
        }
        // `GetMovieGenreItems`, `GetTvGenreItems`.
        Some(view @ (CollectionType::Moviegenre | CollectionType::Tvgenre)) => {
            query.genre_ids = vec![query.parent_id];
            vec![if view == CollectionType::Moviegenre {
                BaseItemKind::Movie
            } else {
                BaseItemKind::Series
            }]
        }
        // `GetMovieCollections`: every box set, no parent.
        Some(CollectionType::Moviecollection) => {
            query.set_parent(None);
            query.parent_row = None;
            kept_parent = false;
            vec![BaseItemKind::BoxSet]
        }
        _ => return true,
    };
    query.include_item_types = kind;
    query.recursive = true;
    query.user_root_children = false;
    kept_parent
}

/// The kind a row's stored `Type` names, by its last segment.
fn kind_from(row: &BaseItemEntity) -> Option<BaseItemKind> {
    serde_json::from_value(serde_json::Value::String(
        row.type_
            .rsplit('.')
            .next()
            .unwrap_or(&row.type_)
            .to_owned(),
    ))
    .ok()
}

/// A channel, or an item of one: its browse takes the channel's policy, not
/// Folder's.
fn is_channel(row: &BaseItemEntity) -> bool {
    row.type_.rsplit('.').next() == Some("Channel")
        || row
            .channel_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .is_some_and(|id| !id.is_nil())
}

/// The view's `ViewType` and the folder its builder works from: its
/// `DisplayParentId`, else its `ParentId` (`UserView.GetItemsInternal`).
fn user_view_browse_context(view: &BaseItemEntity) -> (Option<CollectionType>, Option<Uuid>) {
    let data: serde_json::Value = view
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str(data).ok())
        .unwrap_or_default();
    let view_type = data
        .get("ViewType")
        .and_then(CollectionType::from_data_member);
    let parent_id = data
        .get("DisplayParentId")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .filter(|id| !id.is_nil())
        .or_else(|| {
            view.parent_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .filter(|id| !id.is_nil())
        });
    (view_type, parent_id)
}

/// These UserView builders use direct item selection rather than Folder policy.
fn user_view_uses_direct_items(view_type: Option<CollectionType>) -> bool {
    matches!(
        view_type,
        Some(
            CollectionType::folders
                | CollectionType::books
                | CollectionType::Tvshowseries
                | CollectionType::Tvgenres
                | CollectionType::Tvgenre
                | CollectionType::Tvlatest
                | CollectionType::Tvnextup
                | CollectionType::Tvresume
                | CollectionType::Tvfavoriteseries
                | CollectionType::Tvfavoriteepisodes
                | CollectionType::Movielatest
                | CollectionType::Movieresume
                | CollectionType::Moviemovies
                | CollectionType::Moviecollection
                | CollectionType::Moviefavorites
                | CollectionType::Moviegenres
                | CollectionType::Moviegenre
        )
    )
}

#[cfg(test)]
mod tests {
    //! Each builder over real rows, through the manager as `/Items` calls it.

    use std::sync::Arc;

    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::library::LibraryManager;
    use ferrofin_traits::options::InternalItemsQuery;
    use rstest::rstest;
    use serde_json::json;
    use uuid::Uuid;

    use crate::test_support::{library_manager_over, save_item, seed_user, test_db};

    const VIEW: Uuid = Uuid::from_u128(0x7E57_0001);
    const SHOWN: Uuid = Uuid::from_u128(0x7E57_0002);
    const GONE: Uuid = Uuid::from_u128(0x7E57_0003);

    fn row(id: Uuid, kind: BaseItemKind) -> BaseItemEntity {
        BaseItemEntity {
            id: guid_to_db(id),
            type_: kind.stored_type_name().expect("stored type").to_owned(),
            name: Some(format!("{kind:?}")),
            is_folder: true,
            ..BaseItemEntity::default()
        }
    }

    fn view(data: &serde_json::Value) -> BaseItemEntity {
        BaseItemEntity {
            data: Some(data.to_string()),
            ..row(VIEW, BaseItemKind::UserView)
        }
    }

    /// The query `/Items?parentId=<view>` reaches the builder with.
    fn browse(recursive: bool, kinds: &[BaseItemKind]) -> InternalItemsQuery {
        InternalItemsQuery {
            parent_id: VIEW,
            recursive,
            user_root_children: !recursive,
            include_item_types: kinds.to_vec(),
            ..InternalItemsQuery::default()
        }
    }

    /// A manager whose only row is `shown`, stored at [`SHOWN`].
    async fn manager_with(shown: Option<BaseItemEntity>) -> Arc<dyn LibraryManager> {
        let db = test_db().await;
        if let Some(shown) = shown {
            save_item(&db, &shown).await;
        }
        library_manager_over(db)
    }

    #[rstest]
    #[case::movies("moviemovies", &[BaseItemKind::Movie])]
    #[case::series("tvshowseries", &[BaseItemKind::Series])]
    #[case::books("books", &[BaseItemKind::Book, BaseItemKind::AudioBook])]
    #[tokio::test]
    async fn a_direct_builder_sets_its_own_kinds_and_recursion(
        #[case] view_type: &str,
        #[case] kinds: &[BaseItemKind],
    ) {
        let manager = manager_with(None).await;
        let mut query = browse(false, &[BaseItemKind::Episode]);
        let answer = manager
            .apply_user_view_builder(&mut query, &view(&json!({ "ViewType": view_type })))
            .await
            .expect("builder");
        assert_eq!(answer, (Some(BaseItemKind::UserView), false));
        assert_eq!(query.include_item_types, kinds);
        assert!(query.recursive && !query.user_root_children);
        assert!(!query.folder_query_recursive);
    }

    /// The favourites, latest, resume and genre builders (v12.4
    /// `UserViewBuilder.cs`) force their own kinds, recursion, filters, order
    /// and limit on top of the request, and stay on the view, whose display
    /// parent scopes the query: `GetFavoriteMovies`/`Series`/`Episodes`
    /// (`IsFavorite`), `GetMovieLatest`/`GetTvLatest` (newest first, at most
    /// 50, a list whose total is its length; TV's leaves out virtual episodes),
    /// `GetMovieResume`/`GetTvResume` (`IsResumable`, last played first) and
    /// `GetMovieGenreItems`/`GetTvGenreItems` (`GenreIds` = the view's own id).
    #[rstest]
    #[case::favorite_movies("moviefavorites", BaseItemKind::Movie)]
    #[case::favorite_series("tvfavoriteseries", BaseItemKind::Series)]
    #[case::favorite_episodes("tvfavoriteepisodes", BaseItemKind::Episode)]
    #[case::movie_latest("movielatest", BaseItemKind::Movie)]
    #[case::tv_latest("tvlatest", BaseItemKind::Episode)]
    #[case::movie_resume("movieresume", BaseItemKind::Movie)]
    #[case::tv_resume("tvresume", BaseItemKind::Episode)]
    #[case::movie_genre("moviegenre", BaseItemKind::Movie)]
    #[case::tv_genre("tvgenre", BaseItemKind::Series)]
    #[tokio::test]
    async fn a_sub_view_builder_forces_its_query(
        #[case] view_type: &str,
        #[case] kind: BaseItemKind,
    ) {
        use ferrofin_model::dto::SortOrder::Descending;
        use ferrofin_model::live_tv::ItemSortBy;
        let manager = manager_with(None).await;
        let asked = InternalItemsQuery {
            limit: Some(3),
            order_by: vec![(ItemSortBy::Name, ferrofin_model::dto::SortOrder::Ascending)],
            enable_total_record_count: true,
            ..browse(false, &[BaseItemKind::Video])
        };
        let mut query = asked.clone();
        let shown = view(&json!({ "ViewType": view_type, "DisplayParentId": SHOWN }));
        let answer = manager
            .apply_user_view_builder(&mut query, &shown)
            .await
            .expect("builder");
        assert_eq!(answer, (Some(BaseItemKind::UserView), false));
        assert_eq!(query.include_item_types, [kind]);
        assert!(query.recursive && !query.user_root_children);
        assert_eq!(query.parent_id, VIEW, "the view's display parent scopes it");
        let favorite = view_type.contains("favorite");
        assert_eq!(query.is_favorite, favorite.then_some(true), "{view_type}");
        let (latest, resume) = (view_type.ends_with("latest"), view_type.ends_with("resume"));
        assert_eq!(query.is_resumable, resume.then_some(true), "{view_type}");
        let first = if resume {
            ItemSortBy::DatePlayed
        } else {
            ItemSortBy::DateCreated
        };
        if latest || resume {
            assert_eq!(
                query.order_by,
                [(first, Descending), (ItemSortBy::SortName, Descending)]
            );
            assert_eq!(query.limit, Some(50));
            assert!(!query.enable_total_record_count);
        } else {
            assert_eq!(query.order_by, asked.order_by);
            assert_eq!(query.limit, asked.limit);
            assert!(query.enable_total_record_count);
        }
        assert_eq!(
            query.is_virtual_item,
            (view_type == "tvlatest").then_some(false)
        );
        let genre = view_type.ends_with("genre");
        assert_eq!(query.genre_ids, if genre { vec![VIEW] } else { Vec::new() });
    }

    /// `GetMovieCollections` lists every box set: no parent, so no parent kind.
    #[tokio::test]
    async fn the_collections_sub_view_lists_box_sets_with_no_parent() {
        let manager = manager_with(None).await;
        let mut query = InternalItemsQuery {
            parent_row: Some(Arc::new(view(&json!({})))),
            ..browse(false, &[])
        };
        let shown = view(&json!({ "ViewType": "moviecollection", "DisplayParentId": SHOWN }));
        let answer = manager
            .apply_user_view_builder(&mut query, &shown)
            .await
            .expect("builder");
        assert_eq!(answer, (None, false));
        assert_eq!(query.include_item_types, [BaseItemKind::BoxSet]);
        assert!(query.parent_id.is_nil() && query.parent_type.is_none());
        assert!(query.parent_row.is_none());
        assert!(query.recursive && !query.user_root_children);
    }

    /// The remaining direct builders leave the request as it came, Folder
    /// policy off, and never delegate: `folders` and the genres views list
    /// views (`user_view_list_result`), and next up
    /// is the TV manager's queue (the items handler).
    #[tokio::test]
    async fn a_direct_builder_without_kinds_keeps_the_request() {
        let manager = manager_with(Some(row(SHOWN, BaseItemKind::Folder))).await;
        for view_type in ["folders", "tvgenres", "tvnextup", "moviegenres"] {
            let before = browse(false, &[]);
            let mut query = before.clone();
            let shown = view(&json!({ "ViewType": view_type, "DisplayParentId": SHOWN }));
            let answer = manager
                .apply_user_view_builder(&mut query, &shown)
                .await
                .expect("builder");
            assert_eq!(answer, (Some(BaseItemKind::UserView), false), "{view_type}");
            assert_eq!(query, before, "{view_type}");
        }
    }

    #[rstest]
    #[case::movies("movies", &[], &[BaseItemKind::Movie])]
    #[case::tv(
        "tvshows",
        &[],
        &[BaseItemKind::Series, BaseItemKind::Season, BaseItemKind::Episode]
    )]
    #[case::requested_kinds_kept("tvshows", &[BaseItemKind::Episode], &[BaseItemKind::Episode])]
    #[tokio::test]
    async fn a_recursive_movies_or_tv_browse_queries_recursively_under_the_view(
        #[case] view_type: &str,
        #[case] requested: &[BaseItemKind],
        #[case] kinds: &[BaseItemKind],
    ) {
        let manager = manager_with(Some(row(SHOWN, BaseItemKind::CollectionFolder))).await;
        let mut query = browse(true, requested);
        let shown = view(&json!({ "ViewType": view_type, "DisplayParentId": SHOWN }));
        let answer = manager
            .apply_user_view_builder(&mut query, &shown)
            .await
            .expect("builder");
        // It reports the library's kind, and stays on the view.
        assert_eq!(answer, (Some(BaseItemKind::CollectionFolder), true));
        assert!(query.folder_query_recursive);
        assert_eq!(query.include_item_types, kinds);
        assert_eq!(query.parent_id, VIEW);
        assert!(query.parent_row.is_none());
    }

    /// A display parent that is gone leaves the view's own kind.
    #[tokio::test]
    async fn a_recursive_movies_browse_of_a_view_whose_library_is_gone_reports_the_view() {
        let manager = manager_with(None).await;
        let mut query = browse(true, &[]);
        let gone = view(&json!({ "ViewType": "movies", "DisplayParentId": GONE }));
        let answer = manager
            .apply_user_view_builder(&mut query, &gone)
            .await
            .expect("builder");
        assert_eq!(answer, (Some(BaseItemKind::UserView), true));
        assert!(query.folder_query_recursive);
        assert_eq!(query.include_item_types, [BaseItemKind::Movie]);
    }

    /// Without `recursive` a movies or TV view is not rewritten: it lists its
    /// sub-views (`user_view_list_result`).
    #[tokio::test]
    async fn a_movies_or_tv_browse_without_recursive_is_not_rewritten() {
        let manager = manager_with(Some(row(SHOWN, BaseItemKind::CollectionFolder))).await;
        for view_type in ["movies", "tvshows"] {
            let before = browse(false, &[]);
            let mut query = before.clone();
            let shown = view(&json!({ "ViewType": view_type, "DisplayParentId": SHOWN }));
            let answer = manager
                .apply_user_view_builder(&mut query, &shown)
                .await
                .expect("builder");
            assert_eq!(answer, (Some(BaseItemKind::UserView), false), "{view_type}");
            assert_eq!(query, before, "{view_type}");
        }
    }

    /// Any other view delegates to its display parent: a direct browse moves
    /// onto that folder, row and all; a recursive one stays on the view with
    /// the folder's kind and policy.
    #[tokio::test]
    async fn the_default_builder_delegates_a_direct_browse_to_its_display_parent() {
        let shown_row = row(SHOWN, BaseItemKind::Folder);
        let manager = manager_with(Some(shown_row.clone())).await;
        for data in [
            json!({ "DisplayParentId": SHOWN }),
            json!({ "ViewType": "music", "DisplayParentId": SHOWN }),
            json!({ "ViewType": "playlists", "DisplayParentId": SHOWN }),
        ] {
            let mut direct = browse(false, &[]);
            let answer = manager
                .apply_user_view_builder(&mut direct, &view(&data))
                .await
                .expect("builder");
            assert_eq!(answer, (Some(BaseItemKind::Folder), true), "{data}");
            assert_eq!(direct.parent_id, SHOWN, "{data}");
            assert_eq!(
                direct.known_parent_row().map(|r| r.id.clone()),
                Some(shown_row.id.clone())
            );
            assert!(!direct.user_root_children && !direct.folder_query_recursive);

            let before = browse(true, &[]);
            let mut recursive = before.clone();
            let answer = manager
                .apply_user_view_builder(&mut recursive, &view(&data))
                .await
                .expect("builder");
            assert_eq!(answer, (Some(BaseItemKind::Folder), true), "{data}");
            assert_eq!(recursive, before, "{data}");
        }
    }

    /// A default view shown over a Season delegates to that Season even for a
    /// recursive browse (its aired selection needs the real row), and never
    /// runs Folder's recursive query.
    #[tokio::test]
    async fn the_default_builder_moves_any_browse_of_a_season_onto_it() {
        let manager = manager_with(Some(row(SHOWN, BaseItemKind::Season))).await;
        for recursive in [false, true] {
            let mut query = browse(recursive, &[]);
            let answer = manager
                .apply_user_view_builder(&mut query, &view(&json!({ "DisplayParentId": SHOWN })))
                .await
                .expect("builder");
            assert_eq!(answer, (Some(BaseItemKind::Season), true));
            assert_eq!(query.parent_id, SHOWN);
            assert!(query.known_parent_row().is_some());
            assert!(!query.folder_query_recursive && !query.user_root_children);
        }
    }

    /// The Playlists view delegates to the playlists folder, whose
    /// `GetItemsInternal` lists its own children when there is no user (an
    /// API key): a recursive browse moves onto the folder too, as a direct
    /// one does. A user's recursive browse stays on the view.
    #[tokio::test]
    async fn the_default_builder_moves_a_userless_browse_onto_the_playlists_folder() {
        for kind in [
            BaseItemKind::PlaylistsFolder,
            BaseItemKind::ManualPlaylistsFolder,
        ] {
            let db = test_db().await;
            save_item(&db, &row(SHOWN, kind)).await;
            let user = Arc::new(seed_user(&db, Uuid::from_u128(0x7E57_0004)).await);
            let manager = library_manager_over(db);
            let shown = view(&json!({ "ViewType": "playlists", "DisplayParentId": SHOWN }));

            let mut userless = browse(true, &[]);
            let answer = manager
                .apply_user_view_builder(&mut userless, &shown)
                .await
                .expect("builder");
            assert_eq!(answer, (Some(kind), true), "{kind:?}");
            assert_eq!(userless.parent_id, SHOWN, "{kind:?}");
            assert!(userless.known_parent_row().is_some(), "{kind:?}");
            assert!(!userless.user_root_children, "{kind:?}");

            let before = InternalItemsQuery {
                user: Some(user),
                ..browse(true, &[])
            };
            let mut signed_in = before.clone();
            let answer = manager
                .apply_user_view_builder(&mut signed_in, &shown)
                .await
                .expect("builder");
            assert_eq!(answer, (Some(kind), true), "{kind:?}");
            assert_eq!(signed_in, before, "{kind:?}");
        }
    }

    /// No `DisplayParentId` (or a nil one): the view's own `ParentId`.
    #[tokio::test]
    async fn the_default_builder_falls_back_to_the_views_parent() {
        let manager = manager_with(Some(row(SHOWN, BaseItemKind::Folder))).await;
        for data in [json!({}), json!({ "DisplayParentId": Uuid::nil() })] {
            let parented = BaseItemEntity {
                parent_id: Some(guid_to_db(SHOWN)),
                ..view(&data)
            };
            let mut query = browse(false, &[]);
            let answer = manager
                .apply_user_view_builder(&mut query, &parented)
                .await
                .expect("builder");
            assert_eq!(answer, (Some(BaseItemKind::Folder), true), "{data}");
            assert_eq!(query.parent_id, SHOWN, "{data}");
        }
    }

    /// A channel, or a folder of one, takes the channel's policy, not Folder's.
    #[tokio::test]
    async fn a_channel_display_parent_takes_no_folder_policy() {
        for shown in [
            row(SHOWN, BaseItemKind::Channel),
            BaseItemEntity {
                channel_id: Some(guid_to_db(GONE)),
                ..row(SHOWN, BaseItemKind::Folder)
            },
        ] {
            let kind = shown.type_.clone();
            let manager = manager_with(Some(shown)).await;
            let mut query = browse(false, &[]);
            let (_, folder_policy) = manager
                .apply_user_view_builder(&mut query, &view(&json!({ "DisplayParentId": SHOWN })))
                .await
                .expect("builder");
            assert!(!folder_policy, "{kind}");
            assert_eq!(query.parent_id, SHOWN, "{kind}");
        }
    }

    /// A view with no parent, or whose display parent is gone, lists as itself
    /// with no Folder policy.
    #[tokio::test]
    async fn an_unparented_view_is_left_as_asked() {
        let manager = manager_with(None).await;
        for data in [
            json!({}),
            json!({ "ViewType": "playlists", "DisplayParentId": GONE }),
        ] {
            for recursive in [false, true] {
                let before = browse(recursive, &[]);
                let mut query = before.clone();
                let answer = manager
                    .apply_user_view_builder(&mut query, &view(&data))
                    .await
                    .expect("builder");
                assert_eq!(answer, (Some(BaseItemKind::UserView), false), "{data}");
                assert_eq!(query, before, "{data}");
            }
        }
    }

    /// A library manager whose user views make sub-views under `metadata`,
    /// named by their localization keys (no localization wired).
    fn listing_manager(
        db: &ferrofin_db::Database,
        metadata: &std::path::Path,
    ) -> crate::library_manager::FerrofinLibraryManager {
        let views = crate::FerrofinUserViewManager::new(crate::test_support::item_repository_over(
            db.clone(),
        ))
        .with_item_store(Arc::new(
            crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone()),
        ))
        .with_metadata_path(metadata)
        .with_id_derivation(crate::item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: None,
        });
        crate::test_support::concrete_library_manager_over(db.clone())
            .with_user_views(Arc::new(views))
    }

    /// The query `/Items?parentId=<view>` reaches `query_items` with.
    fn listing(view: &BaseItemEntity, recursive: bool) -> InternalItemsQuery {
        InternalItemsQuery {
            parent_type: Some(BaseItemKind::UserView),
            parent_row: Some(Arc::new(view.clone())),
            ..browse(recursive, &[])
        }
    }

    /// `GetMovieFolders`/`GetTvView` without `Recursive`: the view's sub-views,
    /// in upstream's order, each with its localization key as its name, its
    /// padded forced sort name, the view's display parent as its own, and
    /// `GetUserSubView`'s id. The same rows on every browse, and `GetResult`
    /// counts every one whatever the request says. A recursive browse is
    /// not this listing.
    #[rstest]
    #[case::movies(
        "movies",
        &["movieresume", "movielatest", "moviemovies", "moviecollection", "moviefavorites", "moviegenres"],
        &["HeaderContinueWatching", "Latest", "Movies", "Collections", "Favorites", "Genres"]
    )]
    #[case::tv(
        "tvshows",
        &["tvresume", "tvnextup", "tvlatest", "tvshowseries", "tvfavoriteseries", "tvfavoriteepisodes", "tvgenres"],
        &["HeaderContinueWatching", "HeaderNextUp", "Latest", "Shows", "HeaderFavoriteShows", "HeaderFavoriteEpisodes", "Genres"]
    )]
    #[tokio::test]
    async fn a_movies_or_tv_view_browsed_without_recursive_lists_its_sub_views(
        #[case] view_type: &str,
        #[case] types: &[&str],
        #[case] names: &[&str],
    ) {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        save_item(&db, &row(SHOWN, BaseItemKind::CollectionFolder)).await;
        let shown = view(&json!({ "ViewType": view_type, "DisplayParentId": SHOWN }));
        save_item(&db, &shown).await;
        let manager = listing_manager(&db, tmp.path());

        let listed = manager
            .query_items(&listing(&shown, false))
            .await
            .expect("listed");
        assert_eq!(
            listed.total_record_count,
            i32::try_from(types.len()).unwrap()
        );
        let mode = crate::item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: None,
        };
        for (index, row) in listed.items.iter().enumerate() {
            let data = crate::item_data::parse_data(row.data.as_deref());
            assert_eq!(data["ViewType"], types[index]);
            assert_eq!(data["DisplayParentId"], SHOWN.simple().to_string());
            assert_eq!(row.name.as_deref(), Some(names[index]));
            assert_eq!(row.sort_name, Some(format!("{index:010}")));
            let id = crate::user_view_manager::sub_view_id(&mode, SHOWN, types[index]);
            assert_eq!(Some(row.id.clone()), id.map(guid_to_db));
        }
        let again = manager
            .query_items(&listing(&shown, false))
            .await
            .expect("listed again");
        let ids = |rows: &[BaseItemEntity]| rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&again.items), ids(&listed.items));

        let paged = manager
            .query_items(&InternalItemsQuery {
                start_index: Some(1),
                limit: Some(2),
                enable_total_record_count: false,
                ..listing(&shown, false)
            })
            .await
            .expect("paged");
        assert_eq!(paged.total_record_count, listed.total_record_count);
        assert_eq!(ids(&paged.items), ids(&listed.items[1..3]));

        let typed = manager
            .query_items(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                ..listing(&shown, false)
            })
            .await
            .expect("filtered");
        assert_eq!(
            typed.total_record_count, 0,
            "Filter keeps only the asked kinds"
        );

        let recursive = manager
            .query_items(&listing(&shown, true))
            .await
            .expect("recursive");
        assert!(
            recursive
                .items
                .iter()
                .all(
                    |row| crate::item_type_lookup::kind_from_type_name(&row.type_)
                        != Some(BaseItemKind::UserView)
                ),
            "a recursive browse queries items, never the sub-views"
        );
    }

    /// `UserView.GetItemsInternal` falls back to the view itself when its
    /// display parent is gone: the sub-views hang off the view.
    #[tokio::test]
    async fn a_view_whose_display_parent_is_gone_lists_its_own_sub_views() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let shown = view(&json!({ "ViewType": "movies", "DisplayParentId": GONE }));
        save_item(&db, &shown).await;
        let manager = listing_manager(&db, tmp.path());
        let listed = manager
            .query_items(&listing(&shown, false))
            .await
            .expect("listed");
        assert_eq!(listed.items.len(), 6);
        let data = crate::item_data::parse_data(listed.items[0].data.as_deref());
        assert_eq!(data["DisplayParentId"], VIEW.simple().to_string());
    }

    /// Without the user-view manager the listing is refused, never empty.
    #[tokio::test]
    async fn a_sub_view_listing_without_user_views_is_refused() {
        let db = test_db().await;
        let shown = view(&json!({ "ViewType": "movies" }));
        let manager = library_manager_over(db);
        assert!(manager.query_items(&listing(&shown, false)).await.is_err());
    }
}
