//! `UserView.GetItemsInternal` (`UserView.cs:96-109`): a browse whose parent
//! is a user view runs through `UserViewBuilder.GetUserItems`, which picks the
//! view's builder by its `ViewType`.

use std::sync::Arc;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager as _;
use ferrofin_traits::options::InternalItemsQuery;
use uuid::Uuid;

use super::FerrofinLibraryManager;

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
            // These simple direct builders (`GetMovieMovies`, `GetTvSeries`,
            // `GetBooks`) set their kinds and recursive mode regardless of the
            // request, then forward explicit collapse to SQL. Their automatic
            // global grouping policy is intentionally absent.
            if let Some(kinds) = match view_type {
                Some(CollectionType::Moviemovies) => Some(vec![BaseItemKind::Movie]),
                Some(CollectionType::Tvshowseries) => Some(vec![BaseItemKind::Series]),
                Some(CollectionType::books) => {
                    Some(vec![BaseItemKind::Book, BaseItemKind::AudioBook])
                }
                _ => None,
            } {
                query.include_item_types = kinds;
                query.recursive = true;
                query.user_root_children = false;
            }
            return Ok((kind_from(view), false));
        }
        if matches!(
            view_type,
            Some(CollectionType::movies | CollectionType::tvshows)
        ) {
            if !query.recursive {
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

    /// Every other direct builder (`folders`, the genre, latest, resume, next
    /// up, collection and favourites views) has no kinds of its own: it
    /// leaves the request as it came, Folder policy off, and never delegates.
    #[tokio::test]
    async fn a_direct_builder_without_kinds_keeps_the_request() {
        let manager = manager_with(Some(row(SHOWN, BaseItemKind::Folder))).await;
        for view_type in [
            "folders",
            "tvgenres",
            "tvgenre",
            "tvlatest",
            "tvnextup",
            "tvresume",
            "tvfavoriteseries",
            "tvfavoriteepisodes",
            "movielatest",
            "movieresume",
            "moviecollection",
            "moviefavorites",
            "moviegenres",
            "moviegenre",
        ] {
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

    /// Without `recursive` a movies or TV view is not rewritten; gl-771 tracks
    /// what that browse should list.
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
}
