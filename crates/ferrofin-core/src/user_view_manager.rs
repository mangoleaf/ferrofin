//! [`FerrofinUserViewManager`] — the concrete [`UserViewManager`].
//!
//! Port of `Emby.Server.Implementations.Library.UserViewManager` (the object-safe
//! subset). The C# manager assembles the per-user "views" — the top-level library
//! folders shown on the home screen — and the "latest" rows under each. It leans
//! on the whole `Folder`/`UserView` object tree; at this seam the views are the
//! persisted [`BaseItemKind::CollectionFolder`] / [`BaseItemKind::UserView`] rows,
//! served by the injected [`ItemRepository`].
//!
//! "Latest" is a faithful port of `GetLatestItems` / `GetItemsForLatestItems`:
//! one newest-first query across every parent (over-fetched `limit * 5`, or
//! the tvshows/music grouped-threshold query), then grouped by each row's
//! `LatestItemsIndexContainer` (episode → series, track → album, photo → photo
//! album) until `limit` groups exist. The per-library collection types come
//! from the injected [`VirtualFolderManager`] (C# `CollectionFolder.CollectionType`).
//!
//! Not ported here: the special "grouped" views (all-movies/all-tv merges),
//! channel views, and per-user view ordering from display preferences. Those
//! layer on top of the row set returned here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::{BaseItemKind, CollectionType, MediaType};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{UserViewManager, VirtualFolderManager};
use ferrofin_traits::options::{
    DtoOptions, InternalItemsQuery, LATEST_ITEMS_FALLBACK_LIMIT, LatestItemsQuery,
};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};

use crate::item_type_lookup;
use crate::kinds;
use crate::user_entity_ext;
use ferrofin_db::Database;
use ferrofin_util::sort_name::create_sort_name;

/// The tail of the Live TV view's `Path`, which is how the view is
/// identified.
///
/// C# `GetNamedView` builds it as `{InternalMetadataPath}/views/livetv`
/// (`LibraryManager.cs:2423`) and names the row from the localized
/// `HeaderLiveTV` string — so the name is "TV en direct" or "ライブTV"
/// depending on the server's language, and only the path is stable. A real
/// 10.11.8 database stores `%MetadataPath%/views/livetv`; Ferrofin resolves the
/// token, hence the suffix match.
pub(crate) const LIVE_TV_VIEW_PATH_SUFFIX: &str = "/views/livetv";

/// Whether `path` is the Live TV view's, in either separator — C# builds it
/// with `Path.Combine`, so a database adopted from a Windows Jellyfin stores
/// `…\\views\\livetv` and a POSIX-only match would silently never fire on
/// exactly the case the gate exists for.
#[must_use]
pub(crate) fn is_live_tv_view_path(path: &str) -> bool {
    path.replace('\\', "/").ends_with(LIVE_TV_VIEW_PATH_SUFFIX)
}

/// The display name of the auto-provisioned playlists media folder
/// (C# `ManualPlaylistsFolder.Name`).
const PLAYLISTS_FOLDER_NAME: &str = "Playlists";

/// The concrete user-view manager.
#[derive(Clone)]
pub struct FerrofinUserViewManager {
    items: Arc<dyn ItemRepository>,
    /// The item store, set by the composition root. When present (together with a
    /// [`playlists_path`](Self::playlists_path)), [`get_media_folders`] lazily
    /// provisions the [`BaseItemKind::ManualPlaylistsFolder`] row on first read —
    /// the same self-healing stance `FerrofinVirtualFolderManager` takes for a
    /// library's `CollectionFolder`. `None` in unit tests keeps the manager
    /// read-only.
    ///
    /// [`get_media_folders`]: UserViewManager::get_media_folders
    persistence: Option<Arc<dyn ItemPersistenceService>>,
    /// The on-disk playlists directory (`{data}/playlists`), the provisioned
    /// folder's `Path`. Only meaningful alongside [`persistence`](Self::persistence).
    playlists_path: Option<PathBuf>,
    /// The per-database item-id derivation mode (see
    /// [`item_type_lookup::IdDerivation`]).
    id_derivation: item_type_lookup::IdDerivation,
    /// The library configuration, for each parent's collection type (C#
    /// `CollectionFolder.CollectionType`, which the latest-items rules key
    /// off). `None` in unit tests: every parent is then a type-less folder.
    virtual_folders: Option<Arc<dyn VirtualFolderManager>>,
    /// The database, for the Live TV gate (see
    /// [`user_entity_ext::live_tv_enabled_for`]). `None` in unit tests: the
    /// Live TV view is then listed unconditionally, as it was before.
    db: Option<Database>,
}

impl std::fmt::Debug for FerrofinUserViewManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinUserViewManager")
            .field("has_item_store", &self.persistence.is_some())
            .field("playlists_path", &self.playlists_path)
            .field("has_virtual_folders", &self.virtual_folders.is_some())
            .finish_non_exhaustive()
    }
}

impl FerrofinUserViewManager {
    /// Creates a user-view manager over the injected item repository.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>) -> Self {
        Self {
            items,
            persistence: None,
            playlists_path: None,
            id_derivation: item_type_lookup::IdDerivation::LegacyLowercase,
            virtual_folders: None,
            db: None,
        }
    }

    /// Attaches the database so the Live TV view can be gated on Live TV
    /// actually being available. Called once by the composition root.
    #[must_use]
    pub fn with_database(mut self, db: Database) -> Self {
        self.db = Some(db);
        self
    }

    /// Attaches the virtual-folder manager so the latest-items rules can read
    /// each library's collection type. Called once by the composition root.
    #[must_use]
    pub fn with_virtual_folders(mut self, virtual_folders: Arc<dyn VirtualFolderManager>) -> Self {
        self.virtual_folders = Some(virtual_folders);
        self
    }

    /// Sets the per-database id-derivation mode. Called once by the
    /// composition root (unit tests keep the legacy default).
    #[must_use]
    pub fn with_id_derivation(mut self, mode: item_type_lookup::IdDerivation) -> Self {
        self.id_derivation = mode;
        self
    }

    /// Attaches the item store and the playlists directory so
    /// [`get_media_folders`](UserViewManager::get_media_folders) can lazily
    /// provision the `ManualPlaylistsFolder` row (and create its directory) on
    /// first read. Called once by the composition root.
    #[must_use]
    pub fn with_playlists_store(
        mut self,
        persistence: Arc<dyn ItemPersistenceService>,
        playlists_path: impl Into<PathBuf>,
    ) -> Self {
        self.persistence = Some(persistence);
        self.playlists_path = Some(playlists_path.into());
        self
    }

    /// Drops the Live TV view unless Live TV is actually available to this
    /// user — C# `UserViewManager.GetUserViews`, whose Live TV arm is guarded
    /// by `_liveTvManager.GetEnabledUsers()`. A server with no tuner
    /// configured has no Live TV, and Jellyfin then omits the view entirely
    /// even though the row is still in the database from when a tuner last
    /// existed. Verified against a live 10.11.8 on an adopted 40k-item
    /// library: it serves 8 views where Ferrofin served 9.
    async fn without_disabled_live_tv(
        &self,
        user_id: Uuid,
        views: Vec<BaseItemEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let Some(db) = &self.db else {
            return Ok(views);
        };
        let user_view = item_type_lookup::stored_type_name(BaseItemKind::UserView);
        let is_live_tv = |v: &BaseItemEntity| {
            Some(v.type_.as_str()) == user_view
                && v.path.as_deref().is_some_and(is_live_tv_view_path)
        };
        if !views.iter().any(is_live_tv) {
            return Ok(views);
        }
        let user_id = ferrofin_db::store::guid_to_db(user_id);
        if user_entity_ext::live_tv_enabled_for(db, &user_id).await? {
            return Ok(views);
        }
        Ok(views.into_iter().filter(|v| !is_live_tv(v)).collect())
    }

    /// The deterministic `ManualPlaylistsFolder` item id (`GetNewItemIdInternal`
    /// over the folder path).
    fn playlists_folder_id(&self, playlists_path: &std::path::Path) -> Option<Uuid> {
        item_type_lookup::derive_item_id_with(
            &self.id_derivation,
            BaseItemKind::ManualPlaylistsFolder,
            &playlists_path.to_string_lossy(),
        )
    }

    /// Upserts the `ManualPlaylistsFolder` row (and its directory) when it is
    /// missing (idempotent). No-op without an item store + playlists path wired.
    ///
    /// Port of Jellyfin's lazy `GetUserRootFolder()` provisioning of its
    /// `ManualPlaylistsFolder` child: the folder is `Name="Playlists"`,
    /// `Path={data}/playlists`, and appears among the media folders.
    async fn ensure_playlists_folder(&self) -> Result<(), ServiceError> {
        let (Some(persistence), Some(playlists_path)) = (&self.persistence, &self.playlists_path)
        else {
            return Ok(());
        };
        let Some(id) = self.playlists_folder_id(playlists_path) else {
            return Ok(());
        };
        if persistence.item_exists(id).await? {
            return Ok(());
        }
        // Create the backing directory (C# `ManualPlaylistsFolder` lives on disk).
        tokio::fs::create_dir_all(playlists_path)
            .await
            .map_err(|e| ServiceError::backend(format!("create playlists directory: {e}")))?;
        let entity = BaseItemEntity {
            // `guid_to_db`, NOT `to_string()`. Jellyfin stores Guid columns
            // UPPERCASE-hyphenated and `BaseItems."Id"` is plain TEXT with no
            // COLLATE NOCASE, so a lowercase id is a different row as far as
            // SQLite is concerned. Writing one here meant the `item_exists`
            // check above — which binds `guid_to_db(id)` — could never see the
            // row it had just written, so EVERY `GET /Library/MediaFolders`
            // re-ran `create_dir_all` plus this upsert through the single
            // writer connection. Under load that serialized the endpoint:
            // 1355 ms p50 and 31% errors in the benchmark against Jellyfin's
            // 0.23 ms. It also leaked into the response — the folder came back
            // with a lowercase `Id` where Jellyfin sends uppercase, which is
            // why the suite scored this operation as diverging from upstream.
            id: ferrofin_db::store::guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::ManualPlaylistsFolder)
                .unwrap_or_default()
                .to_owned(),
            name: Some(PLAYLISTS_FOLDER_NAME.to_owned()),
            sort_name: Some(create_sort_name(PLAYLISTS_FOLDER_NAME)),
            path: Some(playlists_path.to_string_lossy().into_owned()),
            is_folder: true,
            date_created: Some(Utc::now()),
            ..BaseItemEntity::default()
        };
        persistence
            .save_items(std::slice::from_ref(&entity))
            .await?;
        Ok(())
    }
}

#[async_trait]
impl UserViewManager for FerrofinUserViewManager {
    async fn get_user_views(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // The user's top-level views are the library collection folders. Per-user
        // access filtering (which libraries the user may see) rides on the
        // InternalItemsQuery.user field in the full pipeline; the base set is every
        // collection folder / user view, name-sorted.
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::CollectionFolder, BaseItemKind::UserView],
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..Default::default()
        };
        let views = self.items.get_item_list(&query).await?;
        self.without_disabled_live_tv(user_id, views).await
    }

    async fn get_media_folders(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // Jellyfin's LibraryController.GetMediaFolders returns
        // GetUserRootFolder().Children sorted by SortName — the library collection
        // folders plus the auto-provisioned ManualPlaylistsFolder. Provision the
        // playlists folder on first read (lazy, self-healing), then project the
        // user-root child kinds, name-sorted.
        self.ensure_playlists_folder().await?;
        let query = InternalItemsQuery {
            include_item_types: vec![
                BaseItemKind::CollectionFolder,
                BaseItemKind::UserView,
                // Both spellings of the playlists folder: Ferrofin provisions
                // one, an adopted database carries the other, and this list has
                // to mean the same set as the query scope in
                // `item_repository::visible_views`.
                BaseItemKind::ManualPlaylistsFolder,
                BaseItemKind::PlaylistsFolder,
            ],
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..Default::default()
        };
        // Upstream returns `GetUserRootFolder().Children`, which never contains
        // the Live TV view: that one is created parentless under
        // `{InternalMetadataPath}/views/`. Gating it here keeps this list
        // meaning the same set as `item_repository::visible_views`, which the
        // comment above depends on.
        let folders = self.items.get_item_list(&query).await?;
        self.without_disabled_live_tv(user_id, folders).await
    }

    async fn get_latest_items(
        &self,
        query: &LatestItemsQuery,
        options: &DtoOptions,
    ) -> Result<Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)>, ServiceError> {
        let rows = self.items_for_latest_items(query, options).await?;
        // `GetLatestItems`: only media rows group — a folder row, or a request
        // with `GroupItems = false`, is listed on its own.
        let (container_of, mut containers) = if query.group_items {
            self.resolve_index_containers(&rows).await?
        } else {
            (vec![None; rows.len()], HashMap::new())
        };

        let mut list: Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)> = Vec::new();
        let mut slot_of: HashMap<Uuid, usize> = HashMap::new();
        for (row, container_id) in rows.into_iter().zip(container_of) {
            let container_id = if row.is_folder { None } else { container_id };
            match container_id {
                None => list.push((None, vec![row])),
                Some(id) => {
                    if let Some(&slot) = slot_of.get(&id) {
                        list[slot].1.push(row);
                    } else {
                        slot_of.insert(id, list.len());
                        list.push((containers.remove(&id), vec![row]));
                    }
                }
            }
            // `if (list.Count >= request.Limit) break;` — checked after EVERY
            // row, so a later row that would have joined an existing group is
            // never seen once the page is full (`ChildCount` counts what was
            // fetched before the cut, exactly as upstream). A null `Limit`
            // never breaks (C# `int >= int?` is false).
            if query
                .limit
                .is_some_and(|limit| usize::try_from(limit).is_ok_and(|limit| list.len() >= limit))
            {
                break;
            }
        }
        Ok(list)
    }
}

/// C# `GetItemsForLatestItems` over-fetch factor: the SQL is capped at
/// `limit * 5` rows so that grouping `limit` containers out of them has enough
/// material (the upstream constant is inline in the query construction).
const LATEST_OVER_FETCH_FACTOR: i32 = 5;

/// The kinds `GetItemsForLatestItems` excludes when neither item types nor
/// media types narrow the query (the by-name rows, which would otherwise pass
/// an `IsFolder = false` scan of a parent folder).
const LATEST_EXCLUDED_BY_NAME_KINDS: [BaseItemKind; 5] = [
    BaseItemKind::Person,
    BaseItemKind::Studio,
    BaseItemKind::Year,
    BaseItemKind::MusicGenre,
    BaseItemKind::Genre,
];

/// One parent of a latest-items request, classified the way the C# code does
/// with `is ICollectionFolder` / `is UserView` / `is Folder` checks.
pub(crate) struct LatestParent {
    id: Uuid,
    kind: Option<BaseItemKind>,
    /// The library's configured type — only an `ICollectionFolder` has one
    /// (a `UserView`'s `ViewType` is not persisted at this seam, so a view
    /// parent reports `None`, the C# `CollectionType.unknown`-equivalent).
    collection_type: Option<CollectionType>,
}

impl LatestParent {
    /// C# `parent is ICollectionFolder`: a library `CollectionFolder`, or the
    /// plugin-folder line (`ManualPlaylistsFolder : BasePluginFolder :
    /// ICollectionFolder`).
    fn is_collection_folder(&self) -> bool {
        matches!(
            self.kind,
            Some(BaseItemKind::CollectionFolder | BaseItemKind::ManualPlaylistsFolder)
        )
    }

    /// C# `parent is UserView`.
    fn is_user_view(&self) -> bool {
        self.kind == Some(BaseItemKind::UserView)
    }
}

/// The `MediaType`s a latest query spans when no item types were requested —
/// the C# `switch (parent.CollectionType)` over the `ICollectionFolder`
/// parents (so `collection_types` must already be filtered to those):
/// books → Book + Audio, music → Audio, photos/homevideos → Photo + Video,
/// anything else (movies, tvshows, mixed/`None`, …) → Video. Deduplicated in
/// first-seen order (upstream collects into a `HashSet`).
#[must_use]
pub(crate) fn media_types_for(collection_types: &[Option<CollectionType>]) -> Vec<MediaType> {
    let mut out: Vec<MediaType> = Vec::new();
    let mut add = |media: MediaType| {
        if !out.contains(&media) {
            out.push(media);
        }
    };
    for collection_type in collection_types {
        match collection_type {
            Some(CollectionType::books) => {
                add(MediaType::Book);
                add(MediaType::Audio);
            }
            Some(CollectionType::music) => add(MediaType::Audio),
            Some(CollectionType::photos | CollectionType::homevideos) => {
                add(MediaType::Photo);
                add(MediaType::Video);
            }
            _ => add(MediaType::Video),
        }
    }
    out
}

/// The item-type / media-type rules of `GetItemsForLatestItems`: with no
/// requested kinds, a set of `UserView` parents that are ALL movies (tvshows)
/// libraries narrows to `Movie` (`Episode`); still-empty kinds then span the
/// media types of the `ICollectionFolder` parents ([`media_types_for`]).
/// Returns `(include_item_types, media_types)`.
#[must_use]
pub(crate) fn latest_kind_rules(
    parents: &[LatestParent],
    mut include_item_types: Vec<BaseItemKind>,
) -> (Vec<BaseItemKind>, Vec<MediaType>) {
    if include_item_types.is_empty() {
        let views: Vec<&LatestParent> = parents.iter().filter(|p| p.is_user_view()).collect();
        if !views.is_empty() {
            if views
                .iter()
                .all(|v| v.collection_type == Some(CollectionType::movies))
            {
                include_item_types = vec![BaseItemKind::Movie];
            } else if views
                .iter()
                .all(|v| v.collection_type == Some(CollectionType::tvshows))
            {
                include_item_types = vec![BaseItemKind::Episode];
            }
        }
    }
    let media_types = if include_item_types.is_empty() {
        let collection_folder_types: Vec<Option<CollectionType>> = parents
            .iter()
            .filter(|p| p.is_collection_folder())
            .map(|p| p.collection_type)
            .collect();
        media_types_for(&collection_folder_types)
    } else {
        Vec::new()
    };
    (include_item_types, media_types)
}

/// The kinds a latest query excludes: the by-name kinds, but only when
/// neither `include_item_types` nor `media_types` already narrow the rows
/// (C# `excludeItemTypes = includeItemTypes.Length == 0 && mediaTypes.Length
/// == 0 ? […] : []`).
#[must_use]
pub(crate) fn exclude_item_types_for(
    include_item_types: &[BaseItemKind],
    media_types: &[MediaType],
) -> Vec<BaseItemKind> {
    if include_item_types.is_empty() && media_types.is_empty() {
        LATEST_EXCLUDED_BY_NAME_KINDS.to_vec()
    } else {
        Vec::new()
    }
}

impl FerrofinUserViewManager {
    /// The collection type of every configured library keyed by its folder
    /// item id (C# `CollectionFolder.CollectionType`). Empty without a
    /// virtual-folder manager wired (unit tests), which makes every parent a
    /// type-less folder — the `default → Video` arm.
    async fn collection_types_by_id(
        &self,
    ) -> Result<HashMap<Uuid, Option<CollectionType>>, ServiceError> {
        let Some(virtual_folders) = &self.virtual_folders else {
            return Ok(HashMap::new());
        };
        Ok(virtual_folders
            .get_virtual_folders()
            .await?
            .into_iter()
            .filter_map(|vf| {
                let id = vf
                    .item_id
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s).ok())?;
                Some((id, vf.collection_type.and_then(kinds::collection_type_of)))
            })
            .collect())
    }

    /// The parents of a latest-items request (C# `GetItemsForLatestItems`,
    /// first half): the `parent_id` folder when it is one, else the user's
    /// views minus `latest_item_excludes`. Also settles `is_played`, which a
    /// music parent clears — decided on the EXPLICIT parent, before the views
    /// fallback, exactly as upstream orders it. `Ok(None)` is the Channel
    /// early exit (an empty result).
    async fn latest_parents(
        &self,
        query: &LatestItemsQuery,
    ) -> Result<Option<(Vec<LatestParent>, Option<bool>)>, ServiceError> {
        let mut parent_rows: Vec<BaseItemEntity> = Vec::new();
        if let Some(parent_id) = query.parent_id.filter(|id| !id.is_nil())
            && let Some(parent) = self.items.retrieve_item(parent_id).await?
        {
            let kind = item_type_lookup::kind_from_type_name(&parent.type_);
            if kind == Some(BaseItemKind::Channel) {
                // C# hands a Channel parent to the channel manager. Ferrofin
                // has no channel content (`/Channels/Items/Latest` is empty
                // for the same reason), so the channel's latest list is empty.
                return Ok(None);
            }
            // `parentItem is Folder` — the class-level test, not the row's
            // `IsFolder` (a DVD-folder `Video` is not a `Folder`).
            if kind.is_some_and(kinds::is_folder) {
                parent_rows.push(parent);
            }
        }
        let collection_types = self.collection_types_by_id().await?;
        let classify = |row: &BaseItemEntity| -> Option<LatestParent> {
            let id = Uuid::parse_str(&row.id).ok()?;
            let kind = item_type_lookup::kind_from_type_name(&row.type_);
            let collection_type = match kind {
                Some(BaseItemKind::CollectionFolder) => {
                    collection_types.get(&id).copied().flatten()
                }
                Some(BaseItemKind::ManualPlaylistsFolder) => Some(CollectionType::playlists),
                _ => None,
            };
            Some(LatestParent {
                id,
                kind,
                collection_type,
            })
        };
        let mut parents: Vec<LatestParent> = parent_rows.iter().filter_map(classify).collect();

        let mut is_played = query.is_played;
        if parents
            .iter()
            .any(|p| p.is_collection_folder() && p.collection_type == Some(CollectionType::music))
        {
            is_played = None;
        }

        if parents.is_empty() {
            let user_id = query
                .user
                .as_ref()
                .and_then(|u| Uuid::parse_str(&u.id).ok())
                .unwrap_or_default();
            parents = self
                .get_user_views(user_id)
                .await?
                .iter()
                .filter_map(classify)
                .filter(|p| !query.latest_item_excludes.contains(&p.id))
                .collect();
        }
        Ok(Some((parents, is_played)))
    }

    /// Port of `UserViewManager.GetItemsForLatestItems`: resolves the parents,
    /// derives the type / media-type / played rules from them, and runs the
    /// ONE query (plain, or the tvshows/music grouped-threshold form) that
    /// feeds the grouping in [`get_latest_items`](UserViewManager::get_latest_items).
    async fn items_for_latest_items(
        &self,
        query: &LatestItemsQuery,
        options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let limit = query.limit.unwrap_or(LATEST_ITEMS_FALLBACK_LIMIT);
        let Some((parents, is_played)) = self.latest_parents(query).await? else {
            return Ok(Vec::new());
        };
        if parents.is_empty() {
            return Ok(Vec::new());
        }

        let (include_item_types, media_types) =
            latest_kind_rules(&parents, query.include_item_types.clone());
        let exclude_item_types = exclude_item_types_for(&include_item_types, &media_types);

        let mut internal = InternalItemsQuery {
            is_folder: include_item_types.is_empty().then_some(false),
            include_item_types,
            exclude_item_types,
            order_by: vec![
                (ItemSortBy::DateCreated, SortOrder::Descending),
                (ItemSortBy::SortName, SortOrder::Descending),
                (ItemSortBy::ProductionYear, SortOrder::Descending),
            ],
            // NFO-declared missing episodes never count as "new".
            is_virtual_item: Some(false),
            limit: Some(limit.saturating_mul(LATEST_OVER_FETCH_FACTOR)),
            is_played,
            dto_options: options.clone(),
            media_types,
            ..InternalItemsQuery::default()
        };
        if let Some(user) = &query.user {
            internal.set_user(user.clone());
        }
        // `LibraryManager.SetTopParentIdsOrAncestors`: libraries and views are
        // top parents (an index seek on `TopParentId`); any other folder — a
        // series, a season — scopes through the `AncestorIds` closure.
        let parent_ids: Vec<Uuid> = parents.iter().map(|p| p.id).collect();
        if parents
            .iter()
            .all(|p| p.is_collection_folder() || p.is_user_view())
        {
            internal.top_parent_ids = parent_ids;
        } else {
            internal.ancestor_ids = parent_ids;
        }

        if query.group_items {
            // The first typed parent decides: a tvshows/music library takes the
            // grouped-threshold query, with `limit` now capping GROUPS.
            let collection_type = parents
                .iter()
                .filter(|p| p.is_collection_folder() || p.is_user_view())
                .find_map(|p| p.collection_type);
            if let Some(ct @ (CollectionType::tvshows | CollectionType::music)) = collection_type {
                internal.limit = Some(limit);
                return self.items.get_latest_item_list(&internal, ct).await;
            }
        }
        self.items.get_item_list(&internal).await
    }

    /// Resolves each row's `LatestItemsIndexContainer`: the id of the row's
    /// container (or `None`), positionally, plus the container rows keyed by
    /// id. ONE batch fetch of the distinct candidates (an episode's `SeriesId`,
    /// a track's / photo's `ParentId`); a candidate is accepted only when its
    /// kind is the container kind — flat audio straight under a library root
    /// does not group under the `CollectionFolder`. An intermediate `Folder`
    /// (a multi-disc subfolder that was not flattened) or `Season` (an
    /// episode with no `SeriesId`) is walked upward once per distinct row, as
    /// C# `FindParent<T>()` climbs the parent chain.
    async fn resolve_index_containers(
        &self,
        rows: &[BaseItemEntity],
    ) -> Result<(Vec<Option<Uuid>>, HashMap<Uuid, BaseItemEntity>), ServiceError> {
        // (row index, candidate id, the kind the container must have)
        let candidates: Vec<(usize, Uuid, BaseItemKind)> =
            rows.iter()
                .enumerate()
                .filter(|(_, row)| !row.is_folder)
                .filter_map(|(i, row)| {
                    let kind = item_type_lookup::kind_from_type_name(&row.type_)?;
                    let wanted = kinds::latest_items_index_container_kind(kind)?;
                    let parse = |id: Option<&str>| {
                        id.and_then(|id| Uuid::parse_str(id).ok())
                            .filter(|id| !id.is_nil())
                    };
                    // C# `Episode.Series`: `SeriesId`, else `FindSeriesId()` —
                    // `FindParent<Series>()` up the parent chain (a hand-edited or
                    // adopted row may lack the id).
                    let candidate = match kind {
                        BaseItemKind::Episode => parse(row.series_id.as_deref())
                            .or_else(|| parse(row.parent_id.as_deref())),
                        _ => parse(row.parent_id.as_deref()),
                    }?;
                    Some((i, candidate, wanted))
                })
                .collect();
        let mut container_of = vec![None; rows.len()];
        if candidates.is_empty() {
            return Ok((container_of, HashMap::new()));
        }

        let mut distinct: Vec<Uuid> = candidates.iter().map(|(_, id, _)| *id).collect();
        distinct.sort_unstable();
        distinct.dedup();
        let fetched = self
            .items
            .get_item_list(&InternalItemsQuery {
                item_ids: distinct,
                ..InternalItemsQuery::default()
            })
            .await?;
        let mut by_id: HashMap<Uuid, BaseItemEntity> = fetched
            .into_iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row)))
            .collect();
        // Memo of the plain-folder walk: candidate folder id → the container
        // found above it (or none), so each folder is climbed at most once.
        let mut climbed: HashMap<Uuid, Option<Uuid>> = HashMap::new();

        for (index, candidate, wanted) in candidates {
            let candidate_kind = by_id
                .get(&candidate)
                .and_then(|row| item_type_lookup::kind_from_type_name(&row.type_));
            let container = match candidate_kind {
                Some(kind) if kind == wanted => Some(candidate),
                // Intermediate containers climb: an unflattened disc `Folder`
                // above a track/photo, a `Season` above an episode whose
                // `SeriesId` is missing. A library root never does — nothing
                // above a `CollectionFolder` can be the container.
                Some(BaseItemKind::Folder | BaseItemKind::Season) => {
                    if let Some(found) = climbed.get(&candidate) {
                        *found
                    } else {
                        let found = self.climb_to(candidate, wanted).await?;
                        let found_id = found.as_ref().and_then(|row| Uuid::parse_str(&row.id).ok());
                        if let (Some(row), Some(id)) = (found, found_id) {
                            by_id.entry(id).or_insert(row);
                        }
                        climbed.insert(candidate, found_id);
                        found_id
                    }
                }
                _ => None,
            };
            container_of[index] = container;
        }
        Ok((container_of, by_id))
    }

    /// The nearest ancestor of `folder` (itself excluded) whose kind is
    /// `wanted` — C# `BaseItem.FindParent<T>()` over the parent chain.
    async fn climb_to(
        &self,
        folder: Uuid,
        wanted: BaseItemKind,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        let chain = self
            .items
            .get_ancestor_chain(folder)
            .await?
            .unwrap_or_default();
        Ok(chain
            .into_iter()
            .find(|row| item_type_lookup::kind_from_type_name(&row.type_) == Some(wanted)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
    use crate::test_support::{
        seed_item, seed_named_item, seed_user, seed_user_data, set_item_path, test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_model::entities_media::VirtualFolderInfo;
    use rstest::rstest;

    fn manager(db: &Database) -> FerrofinUserViewManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinUserViewManager::new(Arc::new(FerrofinItemRepository::new(db.clone(), lookup)))
    }

    fn manager_with_playlists(
        db: &Database,
        playlists_path: impl Into<PathBuf>,
    ) -> FerrofinUserViewManager {
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        manager(db).with_playlists_store(persistence, playlists_path)
    }

    /// The Live TV view is served only while Live TV is available. An adopted
    /// database keeps the row from whenever a tuner last existed; Jellyfin
    /// still leaves it out of `/Users/{id}/Views` (verified against a live
    /// 10.11.8, which served 8 views where Ferrofin served 9).
    #[tokio::test]
    async fn the_live_tv_view_is_served_only_once_a_tuner_exists() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x5101);
        seed_user(&db, user_id).await;
        let user = guid_to_db(user_id);
        let mut tx = db.writer().begin().await.expect("begin");
        crate::user_entity_ext::seed_defaults(&mut tx, &user)
            .await
            .expect("permissions");
        tx.commit().await.expect("commit");
        seed_named_item(
            &db,
            Uuid::from_u128(0x5102),
            BaseItemKind::CollectionFolder,
            "Movies",
        )
        .await;
        // The view is identified by its path, and its NAME is whatever
        // `HeaderLiveTV` says in the server's language — so the fixture uses a
        // localized name on purpose: matching on "Live TV" would pass here and
        // do nothing on a French server.
        let live_tv = Uuid::from_u128(0x5103);
        seed_named_item(&db, live_tv, BaseItemKind::UserView, "TV en direct").await;
        set_item_path(
            &db,
            live_tv,
            &format!("/meta/views{LIVE_TV_VIEW_PATH_SUFFIX}"),
        )
        .await;
        let manager = manager(&db).with_database(db.clone());

        let names = |views: Vec<BaseItemEntity>| {
            views
                .into_iter()
                .filter_map(|v| v.name)
                .collect::<Vec<String>>()
        };
        assert_eq!(
            names(manager.get_user_views(user_id).await.expect("views")),
            ["Movies"],
            "no tuner configured: no Live TV view"
        );

        db.upsert_live_tv_tuner_host("t1", "http://tuner", "m3u", "{}")
            .await
            .expect("tuner");
        assert_eq!(
            names(manager.get_user_views(user_id).await.expect("views")),
            ["Movies", "TV en direct"],
            "with a tuner the view comes back"
        );
    }

    /// A [`VirtualFolderManager`] serving a canned `(folder item id, type)` list.
    struct FakeFolders(Vec<(Uuid, Option<CollectionTypeOptions>)>);

    #[async_trait]
    impl VirtualFolderManager for FakeFolders {
        async fn get_virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
            Ok(self
                .0
                .iter()
                .map(|(id, ct)| VirtualFolderInfo {
                    item_id: Some(guid_to_db(*id)),
                    collection_type: *ct,
                    ..VirtualFolderInfo::default()
                })
                .collect())
        }
        async fn add_virtual_folder(
            &self,
            _name: &str,
            _collection_type: Option<CollectionTypeOptions>,
            _options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn remove_virtual_folder(&self, _name: &str) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn rename_virtual_folder(
            &self,
            _name: &str,
            _new_name: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn add_media_path(
            &self,
            _virtual_folder_name: &str,
            _path_info: &MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn update_media_path(
            &self,
            _virtual_folder_name: &str,
            _path_info: &MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn remove_media_path(
            &self,
            _virtual_folder_name: &str,
            _path: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
        async fn update_library_options(
            &self,
            _virtual_folder_name: &str,
            _options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            unimplemented!("fake")
        }
    }

    fn manager_with_folders(
        db: &Database,
        folders: Vec<(Uuid, Option<CollectionTypeOptions>)>,
    ) -> FerrofinUserViewManager {
        manager(db).with_virtual_folders(Arc::new(FakeFolders(folders)))
    }

    fn day(n: u32) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(&format!("2024-01-{n:02}T00:00:00Z"))
            .expect("date")
            .with_timezone(&Utc)
    }

    /// A full media row the latest query can see: typed, non-virtual, with a
    /// `DateCreated`, `SortName`, `MediaType`, `TopParentId` and parent.
    struct Row<'a> {
        id: Uuid,
        kind: BaseItemKind,
        name: &'a str,
        created: chrono::DateTime<Utc>,
        top_parent: Uuid,
        parent: Uuid,
        series: Option<(Uuid, &'a str)>,
        album: Option<&'a str>,
        is_folder: bool,
        media_type: Option<&'a str>,
    }

    impl<'a> Row<'a> {
        fn new(
            id: Uuid,
            kind: BaseItemKind,
            name: &'a str,
            created: chrono::DateTime<Utc>,
            library: Uuid,
        ) -> Self {
            let media_type = match kind {
                BaseItemKind::Movie | BaseItemKind::Episode | BaseItemKind::MusicVideo => {
                    Some("Video")
                }
                BaseItemKind::Audio => Some("Audio"),
                BaseItemKind::Photo => Some("Photo"),
                BaseItemKind::Book => Some("Book"),
                _ => None,
            };
            Self {
                id,
                kind,
                name,
                created,
                top_parent: library,
                parent: library,
                series: None,
                album: None,
                is_folder: kinds::is_folder(kind),
                media_type,
            }
        }
        fn under(mut self, parent: Uuid) -> Self {
            self.parent = parent;
            self
        }
        fn of_series(mut self, series: Uuid, name: &'a str) -> Self {
            self.series = Some((series, name));
            self
        }
        fn on_album(mut self, album: &'a str) -> Self {
            self.album = Some(album);
            self
        }
    }

    async fn seed(db: &Database, rows: &[Row<'_>]) {
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        let entities: Vec<BaseItemEntity> = rows
            .iter()
            .map(|r| BaseItemEntity {
                id: guid_to_db(r.id),
                type_: stored_type_name(r.kind).unwrap_or_default().to_owned(),
                name: Some(r.name.to_owned()),
                sort_name: Some(r.name.to_owned()),
                date_created: Some(r.created),
                top_parent_id: Some(guid_to_db(r.top_parent)),
                parent_id: Some(guid_to_db(r.parent)),
                series_id: r.series.map(|(id, _)| guid_to_db(id)),
                series_name: r.series.map(|(_, name)| name.to_owned()),
                album: r.album.map(str::to_owned),
                is_folder: r.is_folder,
                media_type: r.media_type.map(str::to_owned),
                ..BaseItemEntity::default()
            })
            .collect();
        persistence.save_items(&entities).await.expect("seed rows");
    }

    fn uuid(e: &BaseItemEntity) -> Uuid {
        Uuid::parse_str(&e.id).expect("row id")
    }

    /// The `(container id, [row ids])` shape of a result, for assertions.
    fn shape(
        groups: &[(Option<BaseItemEntity>, Vec<BaseItemEntity>)],
    ) -> Vec<(Option<Uuid>, Vec<Uuid>)> {
        groups
            .iter()
            .map(|(c, items)| (c.as_ref().map(uuid), items.iter().map(uuid).collect()))
            .collect()
    }

    async fn latest(
        mgr: &FerrofinUserViewManager,
        query: LatestItemsQuery,
    ) -> Vec<(Option<BaseItemEntity>, Vec<BaseItemEntity>)> {
        mgr.get_latest_items(&query, &DtoOptions::default())
            .await
            .expect("latest")
    }

    const MOVIES: Uuid = Uuid::from_u128(0x101);
    const SHOWS: Uuid = Uuid::from_u128(0x102);
    const MUSIC: Uuid = Uuid::from_u128(0x103);

    #[tokio::test]
    async fn user_views_are_the_collection_folders() {
        let db = test_db().await;
        // Ids avoid 1 (the query translator's placeholder row id).
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        // A regular movie is not a view.
        seed_item(&db, Uuid::from_u128(0x103), BaseItemKind::Movie).await;
        let mgr = manager(&db);

        let views = mgr.get_user_views(Uuid::from_u128(9)).await.expect("views");
        assert_eq!(views.len(), 2);
        assert!(
            views
                .iter()
                .all(|v| v.type_ != *"Movie" && v.name.is_some())
        );
    }

    /// The observed bug: the old port queried each view separately and
    /// flattened them in view-name order, so the Movies view's rows always won
    /// the page. Upstream runs ONE query across the views ordered by
    /// `DateCreated DESC` — the newer episodes (Shows view, sorting after
    /// Movies by name) come first, grouped under their series.
    #[tokio::test]
    async fn latest_is_one_query_across_views_ordered_by_date_created_desc() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let series = Uuid::from_u128(0x201);
        let (m1, m2, e1, e2) = (
            Uuid::from_u128(0x301),
            Uuid::from_u128(0x302),
            Uuid::from_u128(0x303),
            Uuid::from_u128(0x304),
        );
        seed(
            &db,
            &[
                Row::new(series, BaseItemKind::Series, "Series 01", day(1), SHOWS),
                Row::new(m1, BaseItemKind::Movie, "Movie 0001", day(2), MOVIES),
                Row::new(m2, BaseItemKind::Movie, "Movie 0002", day(3), MOVIES),
                Row::new(e1, BaseItemKind::Episode, "S1E1", day(8), SHOWS)
                    .under(series)
                    .of_series(series, "Series 01"),
                Row::new(e2, BaseItemKind::Episode, "S1E2", day(9), SHOWS)
                    .under(series)
                    .of_series(series, "Series 01"),
            ],
        )
        .await;
        let mgr = manager_with_folders(
            &db,
            vec![
                (MOVIES, Some(CollectionTypeOptions::movies)),
                (SHOWS, Some(CollectionTypeOptions::tvshows)),
            ],
        );

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&groups),
            vec![
                (Some(series), vec![e2, e1]),
                (None, vec![m2]),
                (None, vec![m1]),
            ]
        );
    }

    /// Equal `DateCreated` (hard-linked fixtures share one btime) breaks on
    /// `SortName DESC` — Jellyfin's `0500, 0499, …`, not the old `ASC`.
    #[tokio::test]
    async fn latest_breaks_date_created_ties_by_sort_name_desc() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        let ids: Vec<Uuid> = (1..=5).map(|n| Uuid::from_u128(0x400 + n)).collect();
        let names: Vec<String> = (1..=5).map(|n| format!("Movie {n:04}")).collect();
        let rows: Vec<Row<'_>> = ids
            .iter()
            .zip(&names)
            .map(|(id, name)| Row::new(*id, BaseItemKind::Movie, name, day(1), MOVIES))
            .collect();
        seed(&db, &rows).await;
        let mgr = manager(&db);

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                limit: Some(3),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&groups),
            vec![
                (None, vec![ids[4]]),
                (None, vec![ids[3]]),
                (None, vec![ids[2]]),
            ],
            "0005, 0004, 0003"
        );
    }

    /// Grouping counts GROUPS against `limit`, over an over-fetched row set:
    /// three episodes of S1, two of S2 and a movie interleaved by date, limit
    /// 2 → exactly two groups, and the second group is whatever came next in
    /// date order. With `limit * 5 = 10` rows fetched the six seeded rows all
    /// reach the grouping loop — the old port cut the ITEM list at `limit`
    /// first, which could never show more than `limit` rows in total.
    #[tokio::test]
    async fn latest_groups_episodes_under_their_series_and_stops_at_limit_groups() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let (s1, s2) = (Uuid::from_u128(0x501), Uuid::from_u128(0x502));
        let (a1, a2, a3, b1, b2, m) = (
            Uuid::from_u128(0x511),
            Uuid::from_u128(0x512),
            Uuid::from_u128(0x513),
            Uuid::from_u128(0x521),
            Uuid::from_u128(0x522),
            Uuid::from_u128(0x531),
        );
        seed(
            &db,
            &[
                Row::new(s1, BaseItemKind::Series, "S1", day(1), SHOWS),
                Row::new(s2, BaseItemKind::Series, "S2", day(1), SHOWS),
                Row::new(a1, BaseItemKind::Episode, "S1E1", day(9), SHOWS)
                    .under(s1)
                    .of_series(s1, "S1"),
                Row::new(b1, BaseItemKind::Episode, "S2E1", day(8), SHOWS)
                    .under(s2)
                    .of_series(s2, "S2"),
                Row::new(a2, BaseItemKind::Episode, "S1E2", day(7), SHOWS)
                    .under(s1)
                    .of_series(s1, "S1"),
                Row::new(m, BaseItemKind::Movie, "M", day(6), MOVIES),
                Row::new(b2, BaseItemKind::Episode, "S2E2", day(5), SHOWS)
                    .under(s2)
                    .of_series(s2, "S2"),
                Row::new(a3, BaseItemKind::Episode, "S1E3", day(4), SHOWS)
                    .under(s1)
                    .of_series(s1, "S1"),
            ],
        )
        .await;
        let mgr = manager(&db);

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                limit: Some(2),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        // Row order: a1 (S1), b1 (S2) → two groups → the loop breaks right
        // there (upstream checks the count after every row), so a2/m/b2/a3
        // are never seen and S1 keeps ChildCount 1.
        assert_eq!(
            shape(&groups),
            vec![(Some(s1), vec![a1]), (Some(s2), vec![b1])]
        );

        // With room for three groups the later rows join their series.
        let groups = latest(
            &mgr,
            LatestItemsQuery {
                limit: Some(3),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&groups),
            vec![
                (Some(s1), vec![a1, a2]),
                (Some(s2), vec![b1]),
                (None, vec![m]),
            ]
        );
    }

    /// `GroupItems = false` lists every row on its own; a folder row (a series
    /// matched by an explicit `includeItemTypes`) never groups either.
    #[tokio::test]
    async fn latest_does_not_group_folders_or_when_group_items_false() {
        let db = test_db().await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let series = Uuid::from_u128(0x601);
        let (e1, e2) = (Uuid::from_u128(0x611), Uuid::from_u128(0x612));
        seed(
            &db,
            &[
                Row::new(series, BaseItemKind::Series, "S", day(3), SHOWS),
                Row::new(e1, BaseItemKind::Episode, "E1", day(9), SHOWS)
                    .under(series)
                    .of_series(series, "S"),
                Row::new(e2, BaseItemKind::Episode, "E2", day(8), SHOWS)
                    .under(series)
                    .of_series(series, "S"),
            ],
        )
        .await;
        let mgr = manager(&db);

        let ungrouped = latest(
            &mgr,
            LatestItemsQuery {
                group_items: false,
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(shape(&ungrouped), vec![(None, vec![e1]), (None, vec![e2])]);

        // `includeItemTypes=Series` lifts the `IsFolder = false` filter (C#
        // `IsFolder = includeItemTypes.Length == 0 ? false : null`); the
        // series row is a folder, so it is listed as itself.
        let folders = latest(
            &mgr,
            LatestItemsQuery {
                include_item_types: vec![BaseItemKind::Series],
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(shape(&folders), vec![(None, vec![series])]);
    }

    /// Only `Episode`/`Audio`/`Photo` have an index container, and only when
    /// the resolved container really is that kind: a track whose parent is the
    /// library root stands alone, a track under a `MusicAlbum` groups, a music
    /// video never groups (the old handler grouped it under its parent).
    #[tokio::test]
    async fn latest_audio_groups_only_under_a_music_album() {
        let db = test_db().await;
        seed_named_item(&db, MUSIC, BaseItemKind::CollectionFolder, "Music").await;
        let album = Uuid::from_u128(0x701);
        let (flat, t1, t2, mv) = (
            Uuid::from_u128(0x711),
            Uuid::from_u128(0x712),
            Uuid::from_u128(0x713),
            Uuid::from_u128(0x714),
        );
        seed(
            &db,
            &[
                Row::new(album, BaseItemKind::MusicAlbum, "Album", day(1), MUSIC),
                Row::new(flat, BaseItemKind::Audio, "Loose", day(9), MUSIC).on_album("Loose"),
                Row::new(t1, BaseItemKind::Audio, "T1", day(8), MUSIC)
                    .under(album)
                    .on_album("Album"),
                Row::new(t2, BaseItemKind::Audio, "T2", day(7), MUSIC)
                    .under(album)
                    .on_album("Album"),
                Row::new(mv, BaseItemKind::MusicVideo, "MV", day(6), MUSIC).under(album),
            ],
        )
        .await;
        // A music library lists Audio media only (the C# media-type switch),
        // so the music video is reachable only through an explicit
        // `includeItemTypes` — which also lifts the media-type rule and is how
        // the "never groups" half of this test sees the row. The typed parent
        // sends the request down the music grouped-threshold path.
        let mgr = manager_with_folders(&db, vec![(MUSIC, Some(CollectionTypeOptions::music))]);

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                include_item_types: vec![BaseItemKind::Audio, BaseItemKind::MusicVideo],
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&groups),
            vec![
                (None, vec![flat]),
                (Some(album), vec![t1, t2]),
                (None, vec![mv]),
            ]
        );

        // Without the explicit kinds the library's media type (Audio) applies
        // and the music video is simply not "latest music". The grouped
        // threshold also bites: the per-`Album` maxima are day 9 ("Loose")
        // and day 8 ("Album"), so the day-7 track is below the smallest of
        // them and the album collapses to its one newer track.
        let groups = latest(
            &mgr,
            LatestItemsQuery {
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&groups),
            vec![(None, vec![flat]), (Some(album), vec![t1])]
        );
    }

    /// A track inside an unflattened disc `Folder` still finds the album
    /// above it (C# `FindParent<MusicAlbum>()` climbs the chain).
    #[tokio::test]
    async fn latest_audio_climbs_a_plain_folder_to_its_album() {
        let db = test_db().await;
        seed_named_item(&db, MUSIC, BaseItemKind::CollectionFolder, "Music").await;
        let (album, disc) = (Uuid::from_u128(0x801), Uuid::from_u128(0x802));
        let (t1, t2) = (Uuid::from_u128(0x811), Uuid::from_u128(0x812));
        seed(
            &db,
            &[
                Row::new(album, BaseItemKind::MusicAlbum, "Album", day(1), MUSIC),
                Row::new(disc, BaseItemKind::Folder, "Disc 1", day(1), MUSIC).under(album),
                Row::new(t1, BaseItemKind::Audio, "T1", day(8), MUSIC)
                    .under(disc)
                    .on_album("Album"),
                Row::new(t2, BaseItemKind::Audio, "T2", day(7), MUSIC)
                    .under(disc)
                    .on_album("Album"),
            ],
        )
        .await;
        // No virtual-folder manager: a type-less parent takes the plain query
        // (`includeItemTypes` lifts its Video media-type default), so this is
        // the container resolution on the non-grouped SQL path.
        let mgr = manager(&db);

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                include_item_types: vec![BaseItemKind::Audio],
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(shape(&groups), vec![(Some(album), vec![t1, t2])]);
    }

    /// `isPlayed` is a SQL predicate (it needs the user), not a post-filter
    /// over an already-cut page: SIX played movies are newer than the one
    /// unplayed movie and the request over-fetches `limit * 5 = 5` rows, so a
    /// post-filter over the page would return nothing while the SQL predicate
    /// finds the fresh one. A music parent drops the filter entirely.
    #[tokio::test]
    async fn latest_pushes_is_played_into_sql() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, MUSIC, BaseItemKind::CollectionFolder, "Music").await;
        let user = seed_user(&db, Uuid::from_u128(0x9)).await;
        let played: Vec<Uuid> = (1..=6).map(|n| Uuid::from_u128(0x910 + n)).collect();
        let names: Vec<String> = (1..=6).map(|n| format!("Seen {n}")).collect();
        let (fresh, track) = (Uuid::from_u128(0x920), Uuid::from_u128(0x921));
        let mut rows = vec![
            Row::new(fresh, BaseItemKind::Movie, "New", day(2), MOVIES),
            Row::new(track, BaseItemKind::Audio, "Track", day(7), MUSIC).on_album("X"),
        ];
        for (id, name) in played.iter().zip(&names) {
            rows.push(Row::new(*id, BaseItemKind::Movie, name, day(9), MOVIES));
        }
        seed(&db, &rows).await;
        for id in &played {
            seed_user_data(&db, Uuid::from_u128(0x9), *id, true, None).await;
        }
        seed_user_data(&db, Uuid::from_u128(0x9), track, true, None).await;
        let mgr = manager_with_folders(
            &db,
            vec![
                (MOVIES, Some(CollectionTypeOptions::movies)),
                (MUSIC, Some(CollectionTypeOptions::music)),
            ],
        );

        let unplayed = latest(
            &mgr,
            LatestItemsQuery {
                user: Some(user.clone()),
                is_played: Some(false),
                limit: Some(1),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        // The played movies are out; the played TRACK is out too — the views
        // fallback keeps `isPlayed` (the music exemption is decided on the
        // explicit parent only, as upstream).
        assert_eq!(shape(&unplayed), vec![(None, vec![fresh])]);

        let music = latest(
            &mgr,
            LatestItemsQuery {
                user: Some(user),
                parent_id: Some(MUSIC),
                is_played: Some(false),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&music),
            vec![(None, vec![track])],
            "a music parent ignores isPlayed"
        );
    }

    /// `parentId` = a tvshows library takes the grouped-threshold query, and
    /// this is only provable where the plain path would answer differently:
    /// series A has TWELVE same-day episodes — more than the plain path's
    /// `limit * 5 = 10` row cap — so the plain query never reaches series B
    /// at all, while the grouped query (limit caps GROUPS, rows are unpaged
    /// above the threshold) returns all twelve plus B's episode. The
    /// threshold is the smallest of the top-2 maxima (day 8): A's old
    /// episode (day 2) and series C (day 3) fall below it.
    #[tokio::test]
    async fn latest_parent_tvshows_library_uses_grouped_threshold_query() {
        let db = test_db().await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let (sa, sb, sc) = (
            Uuid::from_u128(0xA01),
            Uuid::from_u128(0xA02),
            Uuid::from_u128(0xA03),
        );
        let a_eps: Vec<Uuid> = (1..=12).map(|n| Uuid::from_u128(0xA10 + n)).collect();
        let a_names: Vec<String> = (1..=12).map(|n| format!("A e{n:02}")).collect();
        let (a_old, b_new, c_new) = (
            Uuid::from_u128(0xA1F),
            Uuid::from_u128(0xA21),
            Uuid::from_u128(0xA31),
        );
        let mut rows = vec![
            Row::new(sa, BaseItemKind::Series, "A", day(1), SHOWS),
            Row::new(sb, BaseItemKind::Series, "B", day(1), SHOWS),
            Row::new(sc, BaseItemKind::Series, "C", day(1), SHOWS),
            Row::new(a_old, BaseItemKind::Episode, "A e00", day(2), SHOWS)
                .under(sa)
                .of_series(sa, "A"),
            Row::new(b_new, BaseItemKind::Episode, "B e1", day(8), SHOWS)
                .under(sb)
                .of_series(sb, "B"),
            Row::new(c_new, BaseItemKind::Episode, "C e1", day(3), SHOWS)
                .under(sc)
                .of_series(sc, "C"),
        ];
        for (id, name) in a_eps.iter().zip(&a_names) {
            rows.push(
                Row::new(*id, BaseItemKind::Episode, name, day(9), SHOWS)
                    .under(sa)
                    .of_series(sa, "A"),
            );
        }
        seed(&db, &rows).await;
        let mgr = manager_with_folders(&db, vec![(SHOWS, Some(CollectionTypeOptions::tvshows))]);

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                parent_id: Some(SHOWS),
                limit: Some(2),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        // Same-day ties break on SortName DESC: e12, e11, …, e01.
        let mut a_desc = a_eps.clone();
        a_desc.reverse();
        assert_eq!(
            shape(&groups),
            vec![(Some(sa), a_desc), (Some(sb), vec![b_new])]
        );

        // The same request through a type-less parent (no virtual-folder
        // manager) takes the plain path and stops at the 10-row over-fetch —
        // all of it series A — which is what the grouped query exists to fix.
        let plain = latest(
            &manager(&db),
            LatestItemsQuery {
                parent_id: Some(SHOWS),
                limit: Some(2),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].1.len(), 10);
    }

    /// `parentId` = a series row (not a library): scoped through the
    /// `AncestorIds` closure, and — with no typed parent — the by-name kinds
    /// are excluded from the `IsFolder = false` scan.
    #[tokio::test]
    async fn latest_parent_non_library_folder_scopes_by_ancestor_ids() {
        let db = test_db().await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let (series, other) = (Uuid::from_u128(0xB01), Uuid::from_u128(0xB02));
        let (e1, e_other, person) = (
            Uuid::from_u128(0xB11),
            Uuid::from_u128(0xB21),
            Uuid::from_u128(0xB31),
        );
        seed(
            &db,
            &[
                Row::new(series, BaseItemKind::Series, "S", day(1), SHOWS),
                Row::new(other, BaseItemKind::Series, "Other", day(1), SHOWS),
                Row::new(e1, BaseItemKind::Episode, "E1", day(9), SHOWS)
                    .under(series)
                    .of_series(series, "S"),
                Row::new(e_other, BaseItemKind::Episode, "O1", day(8), SHOWS)
                    .under(other)
                    .of_series(other, "Other"),
                // A by-name row that happens to descend from the series.
                Row::new(person, BaseItemKind::Person, "Someone", day(7), SHOWS).under(series),
            ],
        )
        .await;
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        for (item, ancestors) in [
            (e1, vec![series, SHOWS]),
            (e_other, vec![other, SHOWS]),
            (person, vec![series, SHOWS]),
        ] {
            persistence
                .set_ancestors(item, &ancestors)
                .await
                .expect("ancestors");
        }
        let mgr = manager(&db);

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                parent_id: Some(series),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        // Only the series' own episode: the other series' episode is outside
        // the ancestor scope, the person is an excluded by-name kind; the
        // episode is a single row so it is listed as itself (its series still
        // resolved as the container).
        assert_eq!(shape(&groups), vec![(Some(series), vec![e1])]);
    }

    /// `PreferenceKind.LatestItemExcludes` drops a view from the parents.
    #[tokio::test]
    async fn latest_honours_latest_item_excludes() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let (m, e) = (Uuid::from_u128(0xC11), Uuid::from_u128(0xC21));
        seed(
            &db,
            &[
                Row::new(m, BaseItemKind::Movie, "M", day(9), MOVIES),
                Row::new(e, BaseItemKind::Episode, "E", day(8), SHOWS),
            ],
        )
        .await;
        let mgr = manager(&db);

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                latest_item_excludes: vec![MOVIES],
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(shape(&groups), vec![(None, vec![e])]);
    }

    /// Stored view ids are uppercase-hyphenated (`guid_to_db`); a `parentId`
    /// must still match the row whatever its casing.
    #[tokio::test]
    async fn latest_parent_scoping_is_case_insensitive() {
        let db = test_db().await;
        // Hex letters, so the stored uppercase form differs from `to_string()`.
        let movies = Uuid::from_u128(0xABCD_EF01);
        let shows = Uuid::from_u128(0xABCD_EF02);
        seed_named_item(&db, movies, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, shows, BaseItemKind::CollectionFolder, "Shows").await;
        let (m, e) = (Uuid::from_u128(0xD11), Uuid::from_u128(0xD21));
        seed(
            &db,
            &[
                Row::new(m, BaseItemKind::Movie, "M", day(9), movies),
                Row::new(e, BaseItemKind::Episode, "E", day(8), shows),
            ],
        )
        .await;
        let mgr = manager(&db);

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                parent_id: Some(movies),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(shape(&groups), vec![(None, vec![m])]);
    }

    #[rstest]
    #[case(vec![Some(CollectionType::books)], vec![MediaType::Book, MediaType::Audio])]
    #[case(vec![Some(CollectionType::music)], vec![MediaType::Audio])]
    #[case(vec![Some(CollectionType::photos)], vec![MediaType::Photo, MediaType::Video])]
    #[case(vec![Some(CollectionType::homevideos)], vec![MediaType::Photo, MediaType::Video])]
    #[case(vec![Some(CollectionType::movies)], vec![MediaType::Video])]
    #[case(vec![Some(CollectionType::tvshows)], vec![MediaType::Video])]
    #[case(vec![None], vec![MediaType::Video])]
    #[case(
        vec![Some(CollectionType::movies), Some(CollectionType::tvshows), Some(CollectionType::music)],
        vec![MediaType::Video, MediaType::Audio]
    )]
    #[case(vec![], vec![])]
    fn media_types_follow_the_csharp_switch(
        #[case] collection_types: Vec<Option<CollectionType>>,
        #[case] expected: Vec<MediaType>,
    ) {
        assert_eq!(media_types_for(&collection_types), expected);
    }

    fn parent(kind: BaseItemKind, collection_type: Option<CollectionType>) -> LatestParent {
        LatestParent {
            id: Uuid::new_v4(),
            kind: Some(kind),
            collection_type,
        }
    }

    /// The `includeItemTypes` narrowing for grouped views: every `UserView`
    /// parent a movies (tvshows) library → `[Movie]` (`[Episode]`), which also
    /// empties the media types; a mixed set of views narrows nothing, and an
    /// explicit request always wins.
    #[rstest]
    #[case(vec![parent(BaseItemKind::UserView, Some(CollectionType::movies))], vec![], vec![BaseItemKind::Movie], vec![])]
    #[case(
        vec![
            parent(BaseItemKind::UserView, Some(CollectionType::tvshows)),
            parent(BaseItemKind::UserView, Some(CollectionType::tvshows)),
        ],
        vec![],
        vec![BaseItemKind::Episode],
        vec![]
    )]
    #[case(
        vec![
            parent(BaseItemKind::UserView, Some(CollectionType::movies)),
            parent(BaseItemKind::UserView, Some(CollectionType::tvshows)),
            parent(BaseItemKind::CollectionFolder, Some(CollectionType::music)),
        ],
        vec![],
        vec![],
        vec![MediaType::Audio]
    )]
    #[case(
        vec![parent(BaseItemKind::UserView, Some(CollectionType::movies))],
        vec![BaseItemKind::Audio],
        vec![BaseItemKind::Audio],
        vec![]
    )]
    #[case(
        vec![parent(BaseItemKind::Series, None)],
        vec![],
        vec![],
        vec![]
    )]
    fn kind_rules_follow_get_items_for_latest_items(
        #[case] parents: Vec<LatestParent>,
        #[case] requested: Vec<BaseItemKind>,
        #[case] expected_kinds: Vec<BaseItemKind>,
        #[case] expected_media: Vec<MediaType>,
    ) {
        assert_eq!(
            latest_kind_rules(&parents, requested),
            (expected_kinds, expected_media)
        );
    }

    #[test]
    fn by_name_kinds_are_excluded_only_when_nothing_else_narrows() {
        assert_eq!(
            exclude_item_types_for(&[], &[]),
            LATEST_EXCLUDED_BY_NAME_KINDS.to_vec()
        );
        assert!(exclude_item_types_for(&[BaseItemKind::Movie], &[]).is_empty());
        assert!(exclude_item_types_for(&[], &[MediaType::Video]).is_empty());
    }

    #[tokio::test]
    async fn media_folders_include_the_provisioned_playlists_folder() {
        let db = test_db().await;
        // Two libraries, one already present.
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let libraries = 2usize;
        let tmp = tempfile::tempdir().expect("tempdir");
        let playlists_path = tmp.path().join("data").join("playlists");
        let mgr = manager_with_playlists(&db, &playlists_path);

        let folders = mgr
            .get_media_folders(Uuid::from_u128(9))
            .await
            .expect("media folders");

        // The user-root children are the libraries plus the auto-provisioned
        // Playlists folder (C# GetUserRootFolder().Children).
        assert_eq!(folders.len(), libraries + 1);
        let playlists = folders
            .iter()
            .find(|f| {
                f.type_ == "Emby.Server.Implementations.Playlists.ManualPlaylistsFolder"
                    && f.name.as_deref() == Some("Playlists")
            })
            .expect("Playlists media folder present");
        assert!(
            playlists
                .path
                .as_deref()
                .is_some_and(|p| p.ends_with("/data/playlists")),
            "playlists path should end with /data/playlists, got {:?}",
            playlists.path
        );
        // The backing directory is created on disk.
        assert!(playlists_path.is_dir());

        // The id is stored the way Jellyfin stores Guid columns: UPPERCASE
        // hyphenated. `BaseItems."Id"` is plain TEXT with no COLLATE NOCASE, so
        // a lowercase id is a different row to SQLite — the existence check
        // below would never match it, and the folder would also come back to
        // clients with a lowercase `Id` where Jellyfin sends uppercase.
        assert_eq!(
            playlists.id,
            playlists.id.to_uppercase(),
            "the provisioned id must be stored in guid_to_db form, got {}",
            playlists.id
        );

        // Provisioning is idempotent — and this checks that the second read
        // does not WRITE, not merely that it does not duplicate. An upsert on
        // the same id can never duplicate, so a row count proves nothing; the
        // bug this guards against re-ran the upsert on every single request and
        // still left exactly one row. Renaming the row out from under the
        // manager makes a rewrite observable: if provisioning runs again it
        // stamps `Name` back to "Playlists".
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        let mut renamed_row = playlists.clone();
        renamed_row.name = Some("SENTINEL".to_owned());
        persistence
            .save_items(std::slice::from_ref(&renamed_row))
            .await
            .expect("rename the provisioned row");

        let again = mgr
            .get_media_folders(Uuid::from_u128(9))
            .await
            .expect("media folders again");
        assert_eq!(again.len(), libraries + 1);
        let renamed = again
            .iter()
            .find(|f| f.id == playlists.id)
            .expect("the provisioned row is still there");
        assert_eq!(
            renamed.name.as_deref(),
            Some("SENTINEL"),
            "a second read re-provisioned the folder — the existence check did \
             not match the row it wrote, so every request pays a filesystem \
             call and a write through the single writer connection"
        );
    }
}
