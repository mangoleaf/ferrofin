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
//! the tvshows/music query of its own), then grouped by each row's
//! `LatestItemsIndexContainer` (episode → series, track → album, photo → photo
//! album) until `limit` groups exist. The per-library collection types come
//! from the injected [`VirtualFolderManager`] (C# `CollectionFolder.CollectionType`).
//!
//! The views follow the user's display preferences: libraries the user grouped
//! (`GroupedFolders`) become one Movies and one TV Shows view
//! (`FerrofinUserViewManager::with_grouped_views`), hidden views
//! (`MyMediaExcludes`) are left out, and the saved order (`OrderedViews`) is
//! kept.
//!
//! The request's `UserViewQuery` is honoured too: `IncludeExternalContent`
//! false leaves out the Live TV view, and a library whose type `PresetViews`
//! names is listed as its shadow view (`GetShadowView`).
//!
//! TODO(parity, open work item gl-298): `IncludeExternalContent` also lists
//! the channels upstream; Ferrofin has no channel seam, so there are none.

use ferrofin_util::directory_path::DirectoryPath;
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::{BaseItemKind, CollectionType, MediaType};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use uuid::Uuid;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{UserManager, UserViewManager, VirtualFolderManager};
use ferrofin_traits::localization::LocalizationManager;
use ferrofin_traits::options::{
    DtoOptions, InternalItemsQuery, LATEST_ITEMS_FALLBACK_LIMIT, LatestItemsQuery, UserViewQuery,
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

/// Whether this row is the Live TV `UserView` — a `UserView` whose `Path` is
/// `{InternalMetadataPath}/views/livetv`. The Live TV view is the one view that
/// is not a child of the user root, so the media-folder list has to drop it.
fn is_live_tv_view_row(row: &BaseItemEntity) -> bool {
    Some(row.type_.as_str()) == item_type_lookup::stored_type_name(BaseItemKind::UserView)
        && row.path.as_deref().is_some_and(is_live_tv_view_path)
}

/// The Live TV `UserView`'s deterministic id under `mode` — free-standing so it
/// can be asserted without a database (see [`FerrofinUserViewManager::live_tv_view_id`]).
///
/// The key is `path + "_namedview_" + viewType` — Jellyfin 12.0's
/// `GetNamedView` (LibraryManager.cs:2997), which keeps the localized display
/// name out of the id. It is the same derivation
/// [`user_view_repository::consolidate_localized_user_views`] folds the
/// 10.11-era name-keyed views onto, so a later boot never recreates a stale one.
///
/// [`user_view_repository::consolidate_localized_user_views`]: crate::user_view_repository::consolidate_localized_user_views
fn live_tv_view_id_with(
    mode: &item_type_lookup::IdDerivation,
    view_path: &std::path::Path,
) -> Option<Uuid> {
    crate::user_view_repository::named_view_id_at(mode, view_path, LIVE_TV_VIEW_TYPE)
}

/// The Live TV view's `CollectionType` member name — `CollectionType.livetv
/// .ToString()`, the folder under `views/` and the tail of the id key.
const LIVE_TV_VIEW_TYPE: &str = "livetv";

/// The display name of the auto-provisioned Live TV view without a
/// localization wired: `HeaderLiveTV` in `en-US`. Since 12.0 it is
/// display-only: the id is keyed on [`LIVE_TV_VIEW_TYPE`].
const LIVE_TV_VIEW_NAME: &str = "Live TV";

/// The display name of the auto-provisioned playlists media folder
/// (C# `PlaylistsFolder.Name`).
pub(crate) const PLAYLISTS_FOLDER_NAME: &str = "Playlists";

/// The type-wide folders view is a derived root browser, never a library.
pub(crate) fn is_folders_view_row(row: &BaseItemEntity) -> bool {
    is_folders_view_fields(&row.type_, row.path.as_deref(), row.data.as_deref())
}

pub(crate) fn is_folders_view_fields(
    type_name: &str,
    path: Option<&str>,
    blob: Option<&str>,
) -> bool {
    if Some(type_name) != item_type_lookup::stored_type_name(BaseItemKind::UserView) {
        return false;
    }
    let data = crate::item_data::parse_data(blob);
    match data.get("ViewType") {
        Some(value) => {
            value.as_i64() == Some(12)
                || value
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case("folders"))
        }
        None => path.is_some_and(|path| path.replace('\\', "/").ends_with("/views/folders")),
    }
}

/// The concrete user-view manager.
#[derive(Clone)]
pub struct FerrofinUserViewManager {
    items: Arc<dyn ItemRepository>,
    configuration: Option<Arc<dyn ServerConfigurationManager>>,
    localization: Option<Arc<dyn LocalizationManager>>,
    /// The item store, set by the composition root, through which the
    /// `UserView` rows this manager provisions are saved: Live TV, Folders,
    /// the per-user Playlists views and grouped libraries. The
    /// [`BaseItemKind::PlaylistsFolder`] row those views hang off is not one
    /// of them: [`AggregateFolderStore::ensure`](crate::AggregateFolderStore::ensure)
    /// provisions it once at startup, as upstream's
    /// `LibraryManager.CreateRootFolder` does. `None` in unit tests.
    persistence: Option<Arc<dyn ItemPersistenceService>>,
    /// The user-root provisioner. `root_children` settles it on first read so
    /// the `UserRootFolder` row those reads browse exists even on a server that
    /// has never resolved `GET /Items/Root`. The store memoizes, so this costs
    /// nothing after the first call. `None` in unit tests.
    user_root: Option<crate::user_root_folder::UserRootFolderStore>,
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
    /// `{InternalMetadataPath}`, under which the Live TV `UserView` row lives at
    /// `views/livetv`. Set by the composition root; `None` in unit tests keeps
    /// the manager from provisioning the view.
    metadata_path: Option<DirectoryPath>,
    /// The user manager — needed to load the [`UserEntity`] whose visibility
    /// rules decide whether a playlists/boxsets library has a visible item
    /// (12.2 `UserViewManager.HasVisibleItem`). Without it (unit tests) the
    /// check is skipped and such views are always listed.
    users: Option<Arc<dyn UserManager>>,
    /// The batched `IsVisible` evaluator. Without it (unit tests) an item the
    /// user-scoped query returns counts as visible.
    visibility: Option<Arc<crate::item_visibility::ItemVisibility>>,
}

impl std::fmt::Debug for FerrofinUserViewManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinUserViewManager")
            .field("has_item_store", &self.persistence.is_some())
            .field("has_user_root", &self.user_root.is_some())
            .field("has_virtual_folders", &self.virtual_folders.is_some())
            .field("metadata_path", &self.metadata_path)
            .finish_non_exhaustive()
    }
}

/// C# `LibraryManager.GetNamedViewId`: a user's view of a library,
/// `GetNewItemId("38_namedview_" + user("N") + parent("N") + viewType,
/// typeof(UserView))`.
pub(crate) fn named_view_id(
    derivation: &item_type_lookup::IdDerivation,
    user_id: Uuid,
    parent_id: Uuid,
    view_type: &str,
) -> Option<Uuid> {
    item_type_lookup::derive_item_id_with(
        derivation,
        BaseItemKind::UserView,
        &format!(
            "38_namedview_{}{}{view_type}",
            user_id.simple(),
            parent_id.simple()
        ),
    )
}

/// C# `LibraryManager.GetNamedViewId(user, Guid.Empty, viewType)`: a user's
/// view grouping several libraries (`GetNamedView(user, name, viewType,
/// sortName)`), `GetNewItemId("38_namedview_" + user("N") + viewType,
/// typeof(UserView))`. It has no parent, so the id has no parent part.
pub(crate) fn grouped_view_id(
    derivation: &item_type_lookup::IdDerivation,
    user_id: Uuid,
    view_type: &str,
) -> Option<Uuid> {
    item_type_lookup::derive_item_id_with(
        derivation,
        BaseItemKind::UserView,
        &format!("38_namedview_{}{view_type}", user_id.simple()),
    )
}

/// A library's collection type as `CollectionFolder.CollectionType` reads it:
/// the `CollectionType` its row's `Data` carries, lower-cased. A mixed library
/// (`mixed.collection`) has none, as upstream parses it.
fn library_collection_type(row: &BaseItemEntity) -> Option<String> {
    crate::item_data::stored_collection_type(row.data.as_deref())
        .filter(|collection_type| collection_type != "mixed")
}

/// C# `LibraryManager.GetShadowViewId`: the shadow view built from a library,
/// `GetNewItemId("38_namedview_" + name + parentId + viewType,
/// typeof(UserView))`, the parent id in its default (`D`) form: the id of
/// [`FerrofinUserViewManager::shadow_view`], which a database adopted from
/// Jellyfin can also hold in users' preferences.
pub(crate) fn shadow_view_id(
    derivation: &item_type_lookup::IdDerivation,
    name: &str,
    parent_id: Uuid,
    view_type: &str,
) -> Option<Uuid> {
    item_type_lookup::derive_item_id_with(
        derivation,
        BaseItemKind::UserView,
        &format!("38_namedview_{name}{}{view_type}", parent_id.hyphenated()),
    )
}

impl FerrofinUserViewManager {
    /// Creates a user-view manager over the injected item repository.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>) -> Self {
        Self {
            items,
            configuration: None,
            localization: None,
            persistence: None,
            user_root: None,
            id_derivation: item_type_lookup::IdDerivation::LegacyLowercase,
            virtual_folders: None,
            db: None,
            users: None,
            visibility: None,
            metadata_path: None,
        }
    }

    /// Wires the live dashboard display settings and server UI localization.
    #[must_use]
    pub fn with_dashboard_configuration(
        mut self,
        configuration: Arc<dyn ServerConfigurationManager>,
        localization: Arc<dyn LocalizationManager>,
    ) -> Self {
        self.configuration = Some(configuration);
        self.localization = Some(localization);
        self
    }

    /// `GetNamedView(name, folders, "")`: localization changes the display name
    /// while the type/path-derived id stays stable. Disabled views remain stored
    /// so the user's hidden-view preferences survive a later re-enable.
    async fn folders_view(&self) -> Result<Option<BaseItemEntity>, ServiceError> {
        let (Some(configuration), Some(persistence), Some(metadata_path)) =
            (&self.configuration, &self.persistence, &self.metadata_path)
        else {
            return Ok(None);
        };
        if !configuration.configuration().await?.enable_folder_view {
            return Ok(None);
        }
        // Source root Children includes plugin folders even when MediaFolders
        // has never been requested. Settle that same root on the first Folders
        // view read; the existing stores keep warm reads idempotent.
        if let Some(root) = &self.user_root {
            root.ensure().await?;
        }
        let metadata_path = metadata_path.resolve();
        let view_path = crate::user_view_repository::named_view_path(&metadata_path, "folders");
        let Some(id) = crate::user_view_repository::named_view_id(
            &self.id_derivation,
            &metadata_path,
            "folders",
        ) else {
            return Ok(None);
        };
        let name = self.localization.as_ref().map_or_else(
            || "Folders".to_owned(),
            |localization| localization.get_server_localized_string("Folders"),
        );
        let mut previous = self.items.retrieve_item(id).await?;
        if let Some(row) = &mut previous
            && let Some(path) = &row.path
        {
            // BaseItemRepository.Map expands virtual paths upstream, and so
            // does the item repository the composition root wires (its rows
            // come back expanded, so this is the identity there). A
            // repository built without the server's paths returns the token
            // verbatim; compare the resolved path either way, so a warm
            // adopted view does not get saved every read.
            let path = crate::virtual_paths::replace_ignore_ascii_case(
                path,
                "%MetadataPath%",
                &metadata_path.to_string_lossy(),
            );
            let path = crate::virtual_paths::replace_ignore_ascii_case(
                &path,
                "%AppDataPath%",
                &configuration.application_paths().data_path(),
            );
            row.path = Some(path.replace('\\', "/"));
        }
        let same_path = previous.as_ref().is_some_and(|row| {
            is_folders_view_row(row)
                && row.path.as_deref().is_some_and(|path| {
                    ferrofin_util::string_extensions::equals_ordinal_ignore_case(
                        path,
                        &view_path.to_string_lossy(),
                    )
                })
        });
        if same_path
            && previous
                .as_ref()
                .is_some_and(|row| row.name.as_deref() == Some(name.as_str()))
        {
            return Ok(previous);
        }
        tokio::fs::create_dir_all(&view_path)
            .await
            .map_err(|error| {
                ServiceError::backend(format!("create folders view directory: {error}"))
            })?;
        let mut row = if same_path {
            previous.unwrap_or_default()
        } else {
            BaseItemEntity::default()
        };
        if !same_path {
            let metadata = tokio::fs::metadata(&view_path).await.map_err(|error| {
                ServiceError::backend(format!("read folders view directory dates: {error}"))
            })?;
            row.date_created = metadata
                .created()
                .or_else(|_| metadata.modified())
                .ok()
                .map(chrono::DateTime::<Utc>::from);
            row.date_modified = metadata.modified().ok().map(chrono::DateTime::<Utc>::from);
        }
        row.id = ferrofin_db::store::guid_to_db(id);
        item_type_lookup::stored_type_name(BaseItemKind::UserView)
            .unwrap_or_default()
            .clone_into(&mut row.type_);
        row.name = Some(name.clone());
        row.sort_name = Some(create_sort_name(&name));
        row.forced_sort_name = Some(String::new());
        row.path = Some(view_path.to_string_lossy().into_owned());
        row.parent_id = None;
        row.is_folder = true;
        row.date_created = row.date_created.or_else(|| Some(Utc::now()));
        let mut data = crate::item_data::parse_data(row.data.as_deref());
        data.insert("ViewType".to_owned(), serde_json::json!("folders"));
        row.data = Some(serde_json::Value::Object(data).to_string());
        persistence.save_items(std::slice::from_ref(&row)).await?;
        // SaveItems stamps the stored row; expose that fresh timestamp/Etag on
        // the same response that creates or renames the localized view.
        Ok(self.items.retrieve_item(id).await?.or(Some(row)))
    }

    /// Attaches `{InternalMetadataPath}` so the Live TV `UserView` row can be
    /// provisioned at `views/livetv`. Called once by the composition root.
    #[must_use]
    pub fn with_metadata_path(mut self, metadata_path: impl Into<DirectoryPath>) -> Self {
        self.metadata_path = Some(metadata_path.into());
        self
    }

    /// Wires the user manager (see the `users` field).
    #[must_use]
    pub fn with_users(mut self, users: Arc<dyn UserManager>) -> Self {
        self.users = Some(users);
        self
    }

    /// Shares the library's batched `IsVisible` evaluator for home views and
    /// the user-root children.
    #[must_use]
    pub fn with_visibility(
        mut self,
        visibility: Arc<crate::item_visibility::ItemVisibility>,
    ) -> Self {
        self.visibility = Some(visibility);
        self
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

    /// Attaches the item store, so the views this manager provisions (see the
    /// `persistence` field) can be saved. Called once by the composition root.
    #[must_use]
    pub fn with_item_store(mut self, persistence: Arc<dyn ItemPersistenceService>) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Attaches the user-root provisioner, so
    /// [`get_media_folders`](UserViewManager::get_media_folders) and
    /// [`get_user_root_children`](UserViewManager::get_user_root_children)
    /// settle the `UserRootFolder` row on first read. The playlists plugin
    /// folder they also list is not this store's: startup provisions it.
    /// Called once by the composition root.
    #[must_use]
    pub fn with_user_root(mut self, store: crate::user_root_folder::UserRootFolderStore) -> Self {
        self.user_root = Some(store);
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

    /// The id of the per-user, per-parent named view — C# `GetNamedView(user,
    /// name, parentId, viewType, sortName)` (LibraryManager.cs:3038-3052, 12.0):
    /// `GetNewItemId("38_namedview_" + user("N") + parent("N") + viewType,
    /// typeof(UserView))`. Name-independent since 12.0; the stale
    /// name-keyed rows older servers left behind are ignored, not listed.
    pub(crate) fn user_named_view_id(
        &self,
        user_id: Uuid,
        parent_id: Uuid,
        view_type: &str,
    ) -> Option<Uuid> {
        named_view_id(&self.id_derivation, user_id, parent_id, view_type)
    }

    /// Keeps, of the `UserView` rows in `views`, only the ones `GetUserViews`
    /// would derive for `user_id`: a per-user/per-parent view (one carrying a
    /// `DisplayParentId`) only when its id is [`Self::user_named_view_id`] of
    /// its own `ViewType` and parent, a type-wide view (Live TV) always. A
    /// per-user view without a parent (a `UserId` and no `DisplayParentId`)
    /// groups libraries: it comes from the user's grouping
    /// ([`Self::with_grouped_views`]), never from the table, so a user who
    /// ungrouped or another user does not get it.
    fn only_canonical_user_views(
        &self,
        user_id: Uuid,
        views: Vec<BaseItemEntity>,
    ) -> Vec<BaseItemEntity> {
        let user_view = item_type_lookup::stored_type_name(BaseItemKind::UserView);
        views
            .into_iter()
            .filter(|row| {
                if Some(row.type_.as_str()) != user_view {
                    return true;
                }
                let data = crate::item_data::parse_data(row.data.as_deref());
                let Some(parent) = data
                    .get("DisplayParentId")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|p| Uuid::parse_str(p).ok())
                    .filter(|p| !p.is_nil())
                else {
                    return data
                        .get("UserId")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|u| Uuid::parse_str(u).ok())
                        .is_none_or(|u| u.is_nil());
                };
                let Some(view_type) = data.get("ViewType").and_then(serde_json::Value::as_str)
                else {
                    return true;
                };
                let Ok(id) = Uuid::parse_str(&row.id) else {
                    return false;
                };
                self.user_named_view_id(user_id, parent, view_type) == Some(id)
            })
            .collect()
    }

    /// 12.2 `UserViewManager.GetUserViews` (`UserViewManager.cs:62-73`): a
    /// playlists or boxsets library "only references linked items", so its
    /// view is listed only when [`Self::has_visible_item`] holds for its kind
    /// — an account with no playlist it may see has no Playlists view at all.
    /// Skipped when no user manager is wired.
    async fn without_itemless_linked_libraries(
        &self,
        user: Option<&UserEntity>,
        views: Vec<BaseItemEntity>,
        visible_items: &mut HashMap<BaseItemKind, bool>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let Some(user) = user else {
            return Ok(views);
        };
        let user_view = item_type_lookup::stored_type_name(BaseItemKind::UserView);
        let mut kept = Vec::with_capacity(views.len());
        for row in views {
            // Both kinds of row say what they are in `Data`: a derived view
            // carries `ViewType` + `DisplayParentId`, a `CollectionFolder`
            // carries `CollectionType` (Jellyfin's rows and Ferrofin's own).
            // Reading it here keeps this off the virtual-folder listing, which
            // walks the library root and every options file per call — that
            // cost doubled `/Users/{id}/Views` on the bench when tried.
            // Both read the way their DTOs read them: case-insensitively, and
            // a numeric `ViewType` from an adopted blob by its discriminant.
            let collection_type = if Some(row.type_.as_str()) == user_view {
                crate::user_view_repository::view_type_of(row.data.as_deref())
            } else {
                crate::item_data::stored_collection_type(row.data.as_deref())
            };
            if let Some(kind) = crate::item_visibility::linked_kind(collection_type.as_deref())
                && !self.has_visible_item(kind, user, visible_items).await?
            {
                continue;
            }
            kept.push(row);
        }
        Ok(kept)
    }

    /// 12.2 `UserViewManager.HasVisibleItem` for `GetUserViews`
    /// ([`ItemVisibility::has_visible_root_item`]): whether `user` can see at
    /// least one `kind` (`Playlist` or `BoxSet`) item under the top parents
    /// of their root folders. Memoized in `visible_items`: the answer depends
    /// on the kind alone, so a request pays for it at most once per kind.
    ///
    /// Without a visibility evaluator wired (unit tests over a fake store)
    /// any matching id counts.
    ///
    /// [`ItemVisibility::has_visible_root_item`]: crate::item_visibility::ItemVisibility::has_visible_root_item
    async fn has_visible_item(
        &self,
        kind: BaseItemKind,
        user: &UserEntity,
        visible_items: &mut HashMap<BaseItemKind, bool>,
    ) -> Result<bool, ServiceError> {
        if let Some(known) = visible_items.get(&kind) {
            return Ok(*known);
        }
        let visible = match &self.visibility {
            Some(visibility) => visibility.has_visible_root_item(kind, user).await?,
            None => !self
                .items
                .get_item_ids(&visible_item_query(kind, user, Vec::new()))
                .await?
                .is_empty(),
        };
        visible_items.insert(kind, visible);
        Ok(visible)
    }

    /// 12.2 `UserViewManager.GetUserViews`' grouping (`UserViewManager.cs:79-84,
    /// 96-110`). The libraries `user_id` grouped (`GroupedFolders`) that can be
    /// grouped (`UserView.IsEligibleForGrouping`: movies, tvshows or untyped)
    /// leave `views`. Then for movies and for shows, the grouped libraries of
    /// that type or untyped come back as one view (`GetUserView(parents, …)`):
    /// a single library of exactly that type as itself, else the user's grouped
    /// view ([`Self::grouped_view`]). An untyped library joins both.
    ///
    /// Returns whether anything was grouped, when the list needs sorting by
    /// name again. Without an item store and a metadata path no view can be
    /// made, so nothing is grouped.
    async fn with_grouped_views(
        &self,
        user_id: Uuid,
        views: Vec<BaseItemEntity>,
    ) -> Result<(Vec<BaseItemEntity>, bool), ServiceError> {
        let (Some(db), Some(_), Some(_)) = (&self.db, &self.persistence, &self.metadata_path)
        else {
            return Ok((views, false));
        };
        let grouped = user_entity_ext::cached_guid_preference(
            db,
            &ferrofin_db::store::guid_to_db(user_id),
            ferrofin_db::enums::PreferenceKind::GroupedFolders,
        )
        .await?;
        if grouped.is_empty() {
            return Ok((views, false));
        }
        let collection_folder = item_type_lookup::stored_type_name(BaseItemKind::CollectionFolder);
        let mut kept = Vec::with_capacity(views.len());
        let mut parents: Vec<(BaseItemEntity, Option<String>)> = Vec::new();
        for row in views {
            let collection_type = library_collection_type(&row);
            if Some(row.type_.as_str()) == collection_folder
                && matches!(
                    collection_type.as_deref(),
                    None | Some("movies" | "tvshows")
                )
                && Uuid::parse_str(&row.id).is_ok_and(|id| grouped.contains(&id))
            {
                parents.push((row, collection_type));
            } else {
                kept.push(row);
            }
        }
        if parents.is_empty() {
            return Ok((kept, false));
        }
        for (view_type, name_key) in [("movies", "Movies"), ("tvshows", "TvShows")] {
            let of_type: Vec<&(BaseItemEntity, Option<String>)> = parents
                .iter()
                .filter(|(_, collection_type)| {
                    collection_type.as_deref().is_none_or(|t| t == view_type)
                })
                .collect();
            match of_type.as_slice() {
                [] => {}
                [(library, Some(collection_type))] if collection_type == view_type => {
                    kept.push(library.clone());
                }
                _ => match self.grouped_view(user_id, view_type, name_key).await? {
                    Some(view) => kept.push(view),
                    // No view to put them in: the libraries stay listed on
                    // their own rather than leave the list.
                    None => {
                        for (library, _) in of_type {
                            if !kept.iter().any(|row| row.id == library.id) {
                                kept.push(library.clone());
                            }
                        }
                    }
                },
            }
        }
        Ok((kept, true))
    }

    /// `LibraryManager.GetNamedView(user, name, Guid.Empty, viewType, sortName)`
    /// (`LibraryManager.cs:3054-3083`) for a grouped view: the row with
    /// [`grouped_view_id`], created on first read under
    /// `{InternalMetadataPath}/views/{id N}` with its `ViewType` and `UserId`,
    /// and renamed when the server-localized `name_key` reads differently.
    /// `None` without an item store and a metadata path.
    async fn grouped_view(
        &self,
        user_id: Uuid,
        view_type: &str,
        name_key: &str,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        let (Some(persistence), Some(metadata_path)) = (&self.persistence, &self.metadata_path)
        else {
            return Ok(None);
        };
        let Some(id) = grouped_view_id(&self.id_derivation, user_id, view_type) else {
            return Ok(None);
        };
        let name = self.localization.as_ref().map_or_else(
            || name_key.to_owned(),
            |localization| localization.get_server_localized_string(name_key),
        );
        if let Some(mut row) = self.items.retrieve_item(id).await? {
            if row.name.as_deref() != Some(name.as_str()) {
                row.sort_name = Some(create_sort_name(&name));
                row.name = Some(name);
                persistence.save_items(std::slice::from_ref(&row)).await?;
            }
            return Ok(Some(row));
        }
        let view_path = metadata_path.join("views").join(id.simple().to_string());
        tokio::fs::create_dir_all(&view_path)
            .await
            .map_err(|e| ServiceError::backend(format!("create grouped view directory: {e}")))?;
        let data = serde_json::json!({
            "ViewType": view_type,
            "UserId": user_id.simple().to_string(),
        });
        let entity = BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::UserView)
                .unwrap_or_default()
                .to_owned(),
            sort_name: Some(create_sort_name(&name)),
            name: Some(name),
            path: Some(view_path.to_string_lossy().into_owned()),
            parent_id: None,
            is_folder: true,
            date_created: Some(Utc::now()),
            data: Some(data.to_string()),
            ..BaseItemEntity::default()
        };
        persistence
            .save_items(std::slice::from_ref(&entity))
            .await?;
        Ok(Some(entity))
    }

    /// `GetUserViews`' preset arm (`UserViewManager.cs:87-94`, v12.4), and
    /// `GetUserView(parents, …)`'s for a lone grouped library (`:209-217`): a
    /// library still listed whose collection type `preset_views` names is
    /// listed as its [`Self::shadow_view`] instead. A grouped view is no
    /// library, and the playlists library is a user-specific view before this
    /// arm, so neither is ever a `CollectionFolder` row here.
    async fn with_preset_views(
        &self,
        views: Vec<BaseItemEntity>,
        preset_views: &[CollectionType],
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if preset_views.is_empty() {
            return Ok(views);
        }
        let collection_folder = item_type_lookup::stored_type_name(BaseItemKind::CollectionFolder);
        let mut listed = Vec::with_capacity(views.len());
        for row in views {
            let preset = (Some(row.type_.as_str()) == collection_folder)
                .then(|| library_collection_type(&row))
                .flatten()
                .and_then(|stored| {
                    preset_views
                        .iter()
                        .map(|view_type| view_type.json_name())
                        .find(|name| name.eq_ignore_ascii_case(&stored))
                });
            match preset {
                Some(view_type) => match self.shadow_view(&row, &view_type).await? {
                    Some(view) => listed.push(view),
                    None => listed.push(row),
                },
                None => listed.push(row),
            }
        }
        Ok(listed)
    }

    /// `LibraryManager.GetShadowView(library, viewType, string.Empty)`
    /// (`LibraryManager.cs:3131-3166`, v12.4): the `UserView` with
    /// [`shadow_view_id`] that stands for `library`, created on first read with
    /// the library's `Name` and `Path` (whose directory it creates, and whose
    /// times date it), `ViewType` and `DisplayParentId`, and no parent.
    /// `None` without an item store, or for a library with no id or path.
    ///
    /// TODO(parity, open work item gl-1655): `GetShadowView` also queues the
    /// view's metadata refresh (new, stale, or older than its library's
    /// `DateLastSaved`); TODO(parity, open work item gl-1656): the refresh
    /// draws its collage.
    async fn shadow_view(
        &self,
        library: &BaseItemEntity,
        view_type: &str,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        let Some(persistence) = &self.persistence else {
            return Ok(None);
        };
        let (Ok(parent_id), Some(path)) = (Uuid::parse_str(&library.id), library.path.as_deref())
        else {
            return Ok(None);
        };
        let name = library.name.clone().unwrap_or_default();
        let Some(id) = shadow_view_id(&self.id_derivation, &name, parent_id, view_type) else {
            return Ok(None);
        };
        if let Some(row) = self.items.retrieve_item(id).await? {
            return Ok(Some(row));
        }
        // `Directory.CreateDirectory(path)`, whose `CreationTimeUtc` and
        // `LastWriteTimeUtc` become the view's dates.
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| ServiceError::backend(format!("create shadow view directory: {e}")))?;
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| ServiceError::backend(format!("read shadow view directory: {e}")))?;
        let data = serde_json::json!({
            "ViewType": view_type,
            "DisplayParentId": parent_id.simple().to_string(),
        });
        let entity = BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::UserView)
                .unwrap_or_default()
                .to_owned(),
            // `ForcedSortName = string.Empty`: the sort name comes from the name.
            sort_name: Some(create_sort_name(&name)),
            name: Some(name),
            path: Some(path.to_owned()),
            parent_id: None,
            is_folder: true,
            // `CreationTimeUtc` falls back to the write time without a birth
            // time, as `folders_view` reads it.
            date_created: metadata
                .created()
                .or_else(|_| metadata.modified())
                .ok()
                .map(chrono::DateTime::<Utc>::from),
            date_modified: metadata.modified().ok().map(chrono::DateTime::<Utc>::from),
            data: Some(data.to_string()),
            ..BaseItemEntity::default()
        };
        persistence
            .save_items(std::slice::from_ref(&entity))
            .await?;
        Ok(Some(entity))
    }

    async fn user_entity(&self, user_id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        match &self.users {
            Some(users) => users.get_user_by_id(user_id).await,
            None => Ok(None),
        }
    }

    /// Materialises the user's playlists view when a playlists folder exists
    /// and the canonical row does not — the `isNew` branch of `GetNamedView`
    /// (LibraryManager.cs:3069-3088, v12.4): `Path =
    /// {InternalMetadataPath}/views/{id N}`, `Name = folder.Name`, `ViewType =
    /// playlists`, `UserId`, `DisplayParentId = folder.Id`; and renames a row
    /// whose name is not the folder's, `GetNamedView`'s other branch
    /// (`:3089-3093`). The row is looked for among `listed`, the view rows the caller
    /// has just read, which hold it when it exists. Returns whether a row was
    /// written. Idempotent; a no-op without an item store and a metadata path.
    async fn ensure_user_playlists_view(
        &self,
        user_id: Uuid,
        user: Option<&UserEntity>,
        visible_items: &mut HashMap<BaseItemKind, bool>,
        listed: &[BaseItemEntity],
    ) -> Result<bool, ServiceError> {
        let (Some(persistence), Some(metadata_path)) = (&self.persistence, &self.metadata_path)
        else {
            return Ok(false);
        };
        let folders = self
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![
                    BaseItemKind::PlaylistsFolder,
                    BaseItemKind::ManualPlaylistsFolder,
                ],
                ..Default::default()
            })
            .await?;
        let mut written = false;
        for folder in folders {
            let Ok(parent) = Uuid::parse_str(&folder.id) else {
                continue;
            };
            let Some(id) = self.user_named_view_id(user_id, parent, "playlists") else {
                continue;
            };
            let stored_id = ferrofin_db::store::guid_to_db(id);
            let name = folder
                .name
                .clone()
                .unwrap_or_else(|| PLAYLISTS_FOLDER_NAME.to_owned());
            let stored = listed.iter().find(|row| row.id == stored_id);
            if stored.is_some_and(|row| row.name.as_deref() == Some(name.as_str())) {
                continue;
            }
            // 12.2: the folder is skipped before `GetNamedView`, so a user
            // with no playlist to see never gets the view row, nor its rename.
            if let Some(user) = user
                && !self
                    .has_visible_item(BaseItemKind::Playlist, user, visible_items)
                    .await?
            {
                continue;
            }
            if let Some(row) = stored {
                // `BaseItem.Name`'s setter drops the cached `SortName`, so
                // upstream's save writes one made from the new name.
                let mut row = row.clone();
                row.sort_name = Some(create_sort_name(&name));
                row.name = Some(name);
                persistence.save_items(std::slice::from_ref(&row)).await?;
                written = true;
                continue;
            }
            let view_path = metadata_path.join("views").join(id.simple().to_string());
            tokio::fs::create_dir_all(&view_path).await.map_err(|e| {
                ServiceError::backend(format!("create playlists view directory: {e}"))
            })?;
            let data = serde_json::json!({
                "ViewType": "playlists",
                "DisplayParentId": parent.simple().to_string(),
                "UserId": user_id.simple().to_string(),
            });
            let entity = BaseItemEntity {
                id: stored_id,
                type_: item_type_lookup::stored_type_name(BaseItemKind::UserView)
                    .unwrap_or_default()
                    .to_owned(),
                sort_name: Some(create_sort_name(&name)),
                name: Some(name),
                path: Some(view_path.to_string_lossy().into_owned()),
                parent_id: None,
                is_folder: true,
                date_created: Some(Utc::now()),
                data: Some(data.to_string()),
                ..BaseItemEntity::default()
            };
            persistence
                .save_items(std::slice::from_ref(&entity))
                .await?;
            written = true;
        }
        Ok(written)
    }

    /// The deterministic Live TV `UserView` item id — `GetNewItemId(path +
    /// "_namedview_" + viewType, typeof(UserView))` over
    /// `{InternalMetadataPath}/views/livetv` (Jellyfin 12.0; 10.11 keyed the
    /// localized name instead, and those rows are consolidated at boot).
    ///
    /// `GetNewItemIdInternal` strips the program-data path before hashing, so
    /// this id is the SAME on two servers with different data directories.
    fn live_tv_view_id(&self, view_path: &std::path::Path) -> Option<Uuid> {
        live_tv_view_id_with(&self.id_derivation, view_path)
    }

    /// Upserts the Live TV `UserView` row (and its directory) when Live TV is
    /// available to this user (idempotent), looking for it among `listed`, the
    /// view rows the caller has just read. Returns whether a row was written.
    /// No-op without an item store, a metadata path and a database wired.
    ///
    /// Port of the Live TV arm of `UserViewManager.GetUserViews`
    /// (UserViewManager.cs:128-133): when the user is in
    /// `_liveTvManager.GetEnabledUsers()` it appends
    /// `GetInternalLiveTvFolder()`, which is `GetNamedView(name,
    /// CollectionType.livetv, name)` (LiveTvManager.cs:1263) — and
    /// `GetNamedView` (LibraryManager.cs:2856-2898) CREATES the directory and
    /// the row on first read. Ferrofin only ever read such a row, so the view
    /// never existed and `/UserViews` served one fewer view than Jellyfin.
    async fn ensure_live_tv_view(
        &self,
        user_id: Uuid,
        listed: &[BaseItemEntity],
    ) -> Result<bool, ServiceError> {
        let Some(db) = &self.db else {
            return Ok(false);
        };
        // The C# `GetEnabledUsers()` guard: a user without Live TV access never
        // materializes the view.
        if !user_entity_ext::live_tv_enabled_for(db, &ferrofin_db::store::guid_to_db(user_id))
            .await?
        {
            return Ok(false);
        }
        Ok(self
            .provision_live_tv_view(Some(listed))
            .await?
            .is_some_and(|(_, written)| written))
    }

    /// Provisions the Live TV `UserView` row (and its directory) if absent —
    /// the user-less half of [`Self::ensure_live_tv_view`].
    ///
    /// This is what `LiveTvManager.GetInternalLiveTvFolder()` does: it takes no
    /// user, because the folder is the parent every channel ITEM is stored
    /// under, and item rows exist whether or not a given user may see them.
    /// `None` means the manager has no item store or metadata path wired;
    /// otherwise the id and whether a row was written. With `listed`, the view
    /// rows the caller has read, the row is looked for there instead of by id.
    async fn provision_live_tv_view(
        &self,
        listed: Option<&[BaseItemEntity]>,
    ) -> Result<Option<(Uuid, bool)>, ServiceError> {
        let (Some(persistence), Some(metadata_path)) = (&self.persistence, &self.metadata_path)
        else {
            return Ok(None);
        };
        let view_path = metadata_path.join("views").join("livetv");
        let Some(id) = self.live_tv_view_id(&view_path) else {
            return Ok(None);
        };
        // v12.2 `GetInternalLiveTvFolder` (LiveTvManager.cs:1271-1275): the
        // server-localized `HeaderLiveTV`, as name and forced sort name, and
        // `GetNamedView` renames the row when that reads differently
        // (LibraryManager.cs:3036-3042).
        let name = self.localization.as_ref().map_or_else(
            || LIVE_TV_VIEW_NAME.to_owned(),
            |localization| localization.get_server_localized_string("HeaderLiveTV"),
        );
        let stored = match listed {
            Some(rows) => {
                let stored_id = ferrofin_db::store::guid_to_db(id);
                rows.iter().find(|row| row.id == stored_id).cloned()
            }
            None => self.items.retrieve_item(id).await?,
        };
        if let Some(mut row) = stored {
            let renamed = row.name.as_deref() != Some(name.as_str());
            if renamed {
                row.sort_name = Some(ferrofin_util::sort_name::forced_sort_key(&name));
                row.forced_sort_name = Some(name.clone());
                row.name = Some(name);
                persistence.save_items(std::slice::from_ref(&row)).await?;
            }
            return Ok(Some((id, renamed)));
        }
        // C# `Directory.CreateDirectory(path)` inside `GetNamedView`.
        tokio::fs::create_dir_all(&view_path)
            .await
            .map_err(|e| ServiceError::backend(format!("create live tv view directory: {e}")))?;
        let entity = BaseItemEntity {
            // `guid_to_db`, not `to_string()`: `BaseItems."Id"` is plain TEXT
            // with no COLLATE NOCASE, so a lowercase id is a different row.
            id: ferrofin_db::store::guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::UserView)
                .unwrap_or_default()
                .to_owned(),
            // C# `ForcedSortName = sortName` (= the name), and `BaseItem.SortName`
            // then short-circuits to `ModifySortChunks(ForcedSortName)
            // .ToLowerInvariant()` — the digit padding and lower-casing only.
            sort_name: Some(ferrofin_util::sort_name::forced_sort_key(&name)),
            forced_sort_name: Some(name.clone()),
            name: Some(name),
            path: Some(view_path.to_string_lossy().into_owned()),
            // Parentless, exactly as upstream: `GetUserRootFolder().Children`
            // never contains it, which is why `get_media_folders` must not
            // provision it.
            parent_id: None,
            is_folder: true,
            date_created: Some(Utc::now()),
            // `UserView.ViewType` backs `IHasCollectionType.CollectionType`
            // (UserView.cs:58) and is what the DTO projection reads.
            data: Some(r#"{"ViewType":"livetv"}"#.to_owned()),
            ..BaseItemEntity::default()
        };
        persistence
            .save_items(std::slice::from_ref(&entity))
            .await?;
        Ok(Some((id, true)))
    }

    /// One-shot boot repair: folds every 10.11-era `UserView` whose id was
    /// derived from its localized name onto the name-independent id 12.0
    /// derives from the view type — the port of Jellyfin's
    /// `ConsolidateLocalizedUserViews` routine, keyed
    /// [`user_views_consolidated_v12`](crate::user_view_repository::USER_VIEWS_CONSOLIDATED_META_KEY)
    /// in `FerrofinMeta`. Returns the number of stale views dropped. No-op
    /// without a database and a metadata path wired.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when the repair's transaction fails; it then
    /// retries on the next boot.
    pub async fn consolidate_localized_user_views(&self) -> Result<u64, ServiceError> {
        let (Some(db), Some(metadata_path)) = (&self.db, &self.metadata_path) else {
            return Ok(0);
        };
        crate::user_view_repository::consolidate_localized_user_views(
            db,
            &self.id_derivation,
            &metadata_path.resolve(),
        )
        .await
    }
}

#[async_trait]
impl UserViewManager for FerrofinUserViewManager {
    async fn get_user_views(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.get_user_views_with_query(user_id, &UserViewQuery::default())
            .await
    }

    async fn get_user_views_with_query(
        &self,
        user_id: Uuid,
        view_query: &UserViewQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // The user's top-level views are the library collection folders. Per-user
        // access filtering (which libraries the user may see) rides on the
        // InternalItemsQuery.user field in the full pipeline; the base set is every
        // collection folder / user view, name-sorted.
        let user = self.user_entity(user_id).await?;
        let mut visible_items = HashMap::new();
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::CollectionFolder, BaseItemKind::UserView],
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..Default::default()
        };
        let mut views = self.items.get_item_list(&query).await?;
        // Upstream materializes the Live TV view on read, from inside
        // `GetUserViews` itself (UserViewManager.cs:128-133), and the playlists
        // arm (UserViewManager.cs:63-79): a playlists library is user-specific,
        // so its view is `GetNamedView(user, folder.Name, folder.Id,
        // playlists)` — derived, created on demand, never listed from the
        // table. `GetNamedView` looks each row up in its item cache; here a row
        // that exists is among the rows just listed, so only a read that writes
        // one lists them again (gl-1322).
        // `if (query.IncludeExternalContent)` guards the Live TV arm, so a
        // request without external content never materializes the view.
        let live_tv_written = view_query.include_external_content
            && self.ensure_live_tv_view(user_id, &views).await?;
        let playlists_written = self
            .ensure_user_playlists_view(user_id, user.as_ref(), &mut visible_items, &views)
            .await?;
        if live_tv_written || playlists_written {
            views = self.items.get_item_list(&query).await?;
        }
        let views: Vec<_> = self
            .only_canonical_user_views(user_id, views)
            .into_iter()
            .filter(|row| !is_folders_view_row(row))
            .collect();
        let views = if let (Some(visibility), Some(_)) = (&self.visibility, &self.users) {
            let user = user
                .as_ref()
                .ok_or_else(|| ServiceError::not_found("user"))?;
            let visible = visibility.visible(&views, user, false).await?;
            views
                .into_iter()
                .zip(visible)
                .filter_map(|(view, allowed)| allowed.then_some(view))
                .collect()
        } else {
            views
        };
        let views = self
            .without_itemless_linked_libraries(user.as_ref(), views, &mut visible_items)
            .await?;
        let (views, regrouped) = self.with_grouped_views(user_id, views).await?;
        let views = self
            .with_preset_views(views, &view_query.preset_views)
            .await?;
        let mut views = if view_query.include_external_content {
            self.without_disabled_live_tv(user_id, views).await?
        } else {
            views
                .into_iter()
                .filter(|v| !is_live_tv_view_row(v))
                .collect()
        };
        let mut resort = regrouped;
        if let Some(view) = self.folders_view().await? {
            views.push(view);
            resort = true;
        }
        if resort {
            views.sort_by(|a, b| {
                crate::item_repository::folder_ordinal_compare(
                    Some(&crate::item_repository::folder_sort_name(a)),
                    Some(&crate::item_repository::folder_sort_name(b)),
                )
            });
        }
        let views = if view_query.include_hidden {
            views
        } else {
            self.without_hidden_views(user_id, views).await?
        };
        self.in_preferred_order(user_id, views).await
    }

    async fn get_internal_live_tv_folder_id(&self) -> Result<Option<Uuid>, ServiceError> {
        Ok(self.provision_live_tv_view(None).await?.map(|(id, _)| id))
    }

    async fn get_media_folders(&self) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let folders = self.enabled_root_children().await?;
        if folders.len() < 2 {
            return Ok(folders);
        }
        // `Children.Concat(VirtualChildren).OrderBy(i => i.SortName)`: a stable
        // sort by the request culture's `string.CompareTo`, where
        // `root_children` compares ordinally.
        let Ok(comparer) =
            ferrofin_util::current_culture::comparer(&ferrofin_util::current_culture::capture())
        else {
            return Ok(folders);
        };
        // The `Concat` order, which the stable sort keeps on a tie: a library
        // ahead of the playlists folder. `root_children`'s tie order is not it.
        let (libraries, virtual_children): (Vec<_>, Vec<_>) =
            folders.into_iter().partition(|folder| {
                item_type_lookup::kind_from_type_name(&folder.type_)
                    == Some(BaseItemKind::CollectionFolder)
            });
        let mut keyed: Vec<(String, BaseItemEntity)> = libraries
            .into_iter()
            .chain(virtual_children)
            .map(|folder| (crate::item_repository::folder_sort_name(&folder), folder))
            .collect();
        keyed.sort_by(|(a, _), (b, _)| comparer.compare(a, b));
        Ok(keyed.into_iter().map(|(_, folder)| folder).collect())
    }

    async fn get_user_root_children(
        &self,
        user: &UserEntity,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let visibility = self
            .visibility
            .as_ref()
            .ok_or_else(|| ServiceError::backend("item visibility is not configured"))?;
        let folders = self.root_children().await?;
        let visible = visibility.visible(&folders, user, false).await?;
        // `UserRootFolder.GetEligibleChildrenForRecursiveChildren`: the
        // libraries in the `SortName` order `Children` reads them, then
        // `RootFolder.VirtualChildren`, the playlists folder.
        let (libraries, virtual_children): (Vec<_>, Vec<_>) = folders
            .into_iter()
            .zip(visible)
            .filter_map(|(folder, allowed)| allowed.then_some(folder))
            .partition(|folder| {
                item_type_lookup::kind_from_type_name(&folder.type_)
                    == Some(BaseItemKind::CollectionFolder)
            });
        Ok(libraries.into_iter().chain(virtual_children).collect())
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
    /// `IHasCollectionType.CollectionType`, as
    /// [`latest_parent_collection_type`] reads it.
    collection_type: Option<CollectionType>,
}

/// The candidate query behind `UserViewManager.HasVisibleItem`
/// ([`ItemVisibility::has_visible_item`]): every `kind` item under
/// `top_parent_ids`, ungrouped (`GroupByPresentationUniqueKey = false`). No
/// top parents leaves the user's own scope to the repository, which only the
/// evaluator-less fallback of [`FerrofinUserViewManager::has_visible_item`]
/// asks for.
///
/// [`ItemVisibility::has_visible_item`]: crate::item_visibility::ItemVisibility::has_visible_item
pub(crate) fn visible_item_query(
    kind: BaseItemKind,
    user: &UserEntity,
    top_parent_ids: Vec<Uuid>,
) -> InternalItemsQuery {
    InternalItemsQuery {
        include_item_types: vec![kind],
        user: Some(Arc::new(user.clone())),
        top_parent_ids,
        group_by_presentation_unique_key: false,
        checks_playlist_visibility: true,
        ..Default::default()
    }
}

/// The `IHasCollectionType.CollectionType` of a latest-items parent row —
/// the C# `switch (parent.CollectionType)` input.
///
/// - `CollectionFolder` carries the library's own configured type, looked up
///   by id (`CollectionFolder.CollectionType`, CollectionFolder.cs:32);
/// - the playlists plugin folder is the constant `playlists`
///   (`PlaylistsFolder.CollectionType`, v10.11.8
///   `Emby.Server.Implementations/Playlists/PlaylistsFolder.cs:29`) —
///   recognised under BOTH spellings, since this classifies a **stored** row
///   and 10.11.8 persists `…Playlists.PlaylistsFolder` while an older Ferrofin
///   wrote the `GetClientTypeName()` name `ManualPlaylistsFolder`;
/// - a `UserView` is its `ViewType` (`UserView.CollectionType`, UserView.cs:58),
///   which the row keeps in `Data`; a name no `CollectionType` member has reads
///   as `None`, as the DTO's does;
/// - everything else reports `None`, deliberately so for a bare
///   `BasePluginFolder`: the abstract base declares `CollectionType => null`
///   (BasePluginFolder.cs:14) and only `PlaylistsFolder` overrides it, so this
///   arm must NOT be widened to the base class even though
///   [`LatestParent::is_collection_folder`] does recognise it.
///
/// Lifted out of the `classify` closure so it can be pinned directly: getting
/// it wrong loses `CollectionType::playlists` on a playlists parent, which is
/// invisible until the media-type rules downstream quietly change.
fn latest_parent_collection_type(
    kind: Option<BaseItemKind>,
    id: Uuid,
    library_types: &HashMap<Uuid, Option<CollectionType>>,
    data: Option<&str>,
) -> Option<CollectionType> {
    match kind {
        Some(BaseItemKind::CollectionFolder) => library_types.get(&id).copied().flatten(),
        Some(BaseItemKind::ManualPlaylistsFolder | BaseItemKind::PlaylistsFolder) => {
            Some(CollectionType::playlists)
        }
        Some(BaseItemKind::UserView) => crate::user_view_repository::view_type_of(data)
            .and_then(|name| serde_json::from_value(serde_json::Value::String(name)).ok()),
        _ => None,
    }
}

impl LatestParent {
    /// C# `parent is ICollectionFolder`: a library `CollectionFolder`, or the
    /// plugin-folder line (`PlaylistsFolder : BasePluginFolder :
    /// ICollectionFolder`, v10.11.8
    /// `MediaBrowser.Controller/Entities/BasePluginFolder.cs:12`).
    ///
    /// Both spellings, because this classifies a **stored** row: 10.11.8
    /// persists the FQN `…Playlists.PlaylistsFolder`, while
    /// `ManualPlaylistsFolder` is only `GetClientTypeName()` — the spelling an
    /// older Ferrofin wrote. Missing either one drops the parent out of the
    /// `top_parent_ids` branch below and loses `CollectionType::playlists`.
    fn is_collection_folder(&self) -> bool {
        matches!(
            self.kind,
            Some(
                BaseItemKind::CollectionFolder
                    | BaseItemKind::BasePluginFolder
                    | BaseItemKind::ManualPlaylistsFolder
                    | BaseItemKind::PlaylistsFolder
            )
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
/// views narrows to `Movie` (`Episode`); still-empty kinds then span the
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
    /// `UserViewManager.GetUserViews` omits views in the user's
    /// `MyMediaExcludes` preference unless `IncludeHidden` was requested.
    /// This applies only to home views; media folders and latest-item
    /// exclusions are independent preferences.
    async fn without_hidden_views(
        &self,
        user_id: Uuid,
        views: Vec<BaseItemEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let Some(db) = &self.db else {
            return Ok(views);
        };
        let excluded = user_entity_ext::cached_guid_preference(
            db,
            &ferrofin_db::store::guid_to_db(user_id),
            ferrofin_db::enums::PreferenceKind::MyMediaExcludes,
        )
        .await?;
        if excluded.is_empty() {
            return Ok(views);
        }
        Ok(views
            .into_iter()
            .filter(|view| Uuid::parse_str(&view.id).is_ok_and(|id| !excluded.contains(&id)))
            .collect())
    }

    /// `GetUserViews`' last step (`UserViewManager.cs:141-158`): the views in
    /// the order the user saved (`OrderedViews`) — a view by its own id, else
    /// by the `DisplayParentId` it stands for — with every view the list does
    /// not name after them, and ties (all of them, with no saved order) in the
    /// name order the list already has.
    async fn in_preferred_order(
        &self,
        user_id: Uuid,
        mut views: Vec<BaseItemEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let Some(db) = &self.db else {
            return Ok(views);
        };
        let orders = user_entity_ext::cached_guid_preference(
            db,
            &ferrofin_db::store::guid_to_db(user_id),
            ferrofin_db::enums::PreferenceKind::OrderedViews,
        )
        .await?;
        if orders.is_empty() {
            return Ok(views);
        }
        let position = |id: Option<Uuid>| id.and_then(|id| orders.iter().position(|o| *o == id));
        let rank = |view: &BaseItemEntity| {
            position(Uuid::parse_str(&view.id).ok())
                .or_else(|| {
                    let data = crate::item_data::parse_data(view.data.as_deref());
                    let parent = crate::item_data::read_data_string(&data, "DisplayParentId")
                        .and_then(|id| Uuid::parse_str(&id).ok())
                        .filter(|id| !id.is_nil());
                    position(parent)
                })
                .unwrap_or(usize::MAX)
        };
        // A stable sort: views with equal rank keep their name order.
        views.sort_by_key(rank);
        Ok(views)
    }

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
            .library_folders()
            .await?
            .iter()
            .filter_map(|vf| {
                let id = vf
                    .item_id
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s).ok())?;
                Some((id, vf.collection_type.and_then(kinds::collection_type_of)))
            })
            .collect())
    }

    /// [`Self::root_children`] less the libraries whose options disable them
    /// (`GetLibraryOptions(i).Enabled`), in its ordinal `SortName` order.
    async fn enabled_root_children(&self) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let folders = self.root_children().await?;
        // An administrator's list, so a manager with no evaluator only skips
        // the `Enabled` filter; `get_user_root_children` must refuse instead.
        match &self.visibility {
            Some(visibility) => visibility.without_disabled_libraries(folders).await,
            None => Ok(folders),
        }
    }

    /// Every user-root child, whoever asks: Jellyfin's
    /// `GetUserRootFolder().Children` sorted by `SortName` — the library
    /// collection folders plus the playlists plugin folder `CreateRootFolder()`
    /// parents to the `AggregateFolder`. That folder is provisioned once at
    /// startup (`AggregateFolderStore::ensure`), as upstream's `RootFolder`
    /// runs `CreateRootFolder` once per process, so this read never writes it.
    /// Settles the user root on first read (lazy, self-healing, memoized by
    /// the store), then projects the user-root child kinds, name-sorted.
    async fn root_children(&self) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if let Some(root) = &self.user_root {
            root.ensure().await?;
        }
        let query = InternalItemsQuery {
            // `GetUserRootFolder().Children` holds the libraries and the
            // playlists folder — never a `UserView` (those are derived views,
            // not root children; the query scope in
            // `item_repository::visible_views` still admits them).
            include_item_types: vec![
                BaseItemKind::CollectionFolder,
                // Both kinds of playlists folder: Ferrofin and Jellyfin both
                // store `PlaylistsFolder`, but a row an earlier Ferrofin
                // wrote as `ManualPlaylistsFolder` survives when startup
                // finds the correct row beside it and does not re-key it.
                BaseItemKind::ManualPlaylistsFolder,
                BaseItemKind::PlaylistsFolder,
            ],
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..Default::default()
        };
        // Upstream returns `GetUserRootFolder().Children`, which never contains
        // the Live TV view — that one is created parentless under
        // `{InternalMetadataPath}/views/` and only `GetUserViews` appends it.
        // The exclusion is UNCONDITIONAL, not the Live-TV-disabled gate
        // `get_user_views` applies: an enabled tuner puts the view in
        // `/UserViews`, never in `/Library/MediaFolders`.
        let folders = self.items.get_item_list(&query).await?;
        Ok(folders
            .into_iter()
            .filter(|f| !is_live_tv_view_row(f))
            .collect())
    }

    /// The parents of a latest-items request (C# `GetItemsForLatestItems`,
    /// first half): the `parent_id` folder when it is one, else the user's
    /// root media folders minus `latest_item_excludes`. Also settles `is_played`,
    /// which a music parent clears — decided on the EXPLICIT parent, before the
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
            let collection_type =
                latest_parent_collection_type(kind, id, &collection_types, row.data.as_deref());
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
            // No user is an unscoped read, as on `InternalItemsQuery`:
            // `get_latest_media` always names one.
            let folders = match &query.user {
                Some(user) => self.get_user_root_children(user).await?,
                None => self.enabled_root_children().await?,
            };
            parents = folders
                .iter()
                .filter_map(classify)
                .filter(|p| !query.latest_item_excludes.contains(&p.id))
                .collect();
        }
        Ok(Some((parents, is_played)))
    }

    /// Port of `UserViewManager.GetItemsForLatestItems`: resolves the parents,
    /// derives the type / media-type / played rules from them, and reads the
    /// rows that feed the grouping in
    /// [`get_latest_items`](UserViewManager::get_latest_items) — the plain form,
    /// or, for a grouped request whose first typed parent is a library or view
    /// of one of the three short-circuited types, that type's own query (one card per
    /// newest series for tvshows, the newest albums for music, the newest row
    /// of each presentation key for movies), or, when no parent is typed, the
    /// `unknown` arm that merges the movies and tvshows ones.
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
            // The trait hands in `&DtoOptions`: one copy here, as before; the
            // query's own clones then share it.
            dto_options: Arc::new(options.clone()),
            media_types,
            ..InternalItemsQuery::default()
        };
        if let Some(user) = &query.user {
            internal.set_user(user.clone());
        }
        // `LibraryManager.SetTopParentIdsOrAncestors`: libraries are top parents
        // (an index seek on `TopParentId`), and any other folder — a series, a
        // season — scopes through the `AncestorIds` closure. A view stands for
        // the libraries behind it (`GetTopParentIdsForQuery`: a grouped view
        // for each library the user grouped into it), and no row carries one
        // as its `TopParentId`, so it cannot go in `top_parent_ids`. The
        // repository translates it from a recursive parent, as for a browse
        // of the view. Only `parentId` names a view, so it is the sole parent.
        // A folders view stands for no library, and a browse of it lists the
        // root folders, so it keeps upstream's `SetAncestorIds` of itself.
        let parent_ids: Vec<Uuid> = parents.iter().map(|p| p.id).collect();
        if let [view] = parents.as_slice()
            && view.is_user_view()
            && view.collection_type != Some(CollectionType::folders)
        {
            internal.set_parent(Some((view.id, BaseItemKind::UserView)));
            internal.recursive = true;
        } else if parents.iter().all(LatestParent::is_collection_folder) {
            internal.top_parent_ids = parent_ids;
        } else {
            internal.ancestor_ids = parent_ids;
        }

        if query.group_items {
            // The first typed parent decides (v12 `UserViewManager.cs:396-430`,
            // `FirstOrDefault(type => type is not null)`): a tvshows, music or
            // movies library or view short-circuits into its own per-collection-type
            // query, with `limit` now capping GROUPS (tvshows), ALBUMS (music)
            // or presentation keys (movies) rather than rows.
            //
            // "First" is the parents' order, and with no `parentId` the parents
            // are every root folder the user may open — so on a server whose
            // first by `SortName` is a movies library, the UNSCOPED `/Items/Latest`
            // (the home row) is the movies arm too: the newest `limit` rows of
            // EVERY library, one per key, grouped below — a burst of new tracks
            // can fill it with a few album cards. That is upstream's behaviour
            // (the owner chose to follow v12 here on 2026-10-24), not an
            // artifact of this port.
            //
            // No typed parent at all (`:425-429`) takes the `unknown` arm, the
            // newest movies merged with the newest TV cards: a MIXED library,
            // and also any parent that is neither a library nor a view (a
            // folder, a series, a box set), as upstream's `collectionType is
            // null` is.
            //
            // The plain path below, which only a typed parent of another type
            // (books, photos, …) or an ungrouped request still takes, over-fetches
            // 10.11.8's `limit * 5` ([`LATEST_OVER_FETCH_FACTOR`]) where v12
            // fetches `limit * 2` (`:390`); the short-circuits overwrite it on
            // the next line.
            let collection_type = parents
                .iter()
                .filter(|p| p.is_collection_folder() || p.is_user_view())
                .find_map(|p| p.collection_type);
            let arm = match collection_type {
                Some(
                    ct @ (CollectionType::tvshows | CollectionType::music | CollectionType::movies),
                ) => Some(ct),
                None => Some(CollectionType::unknown),
                Some(_) => None,
            };
            if let Some(ct) = arm {
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
        seed_item, seed_item_with_data, seed_named_item, seed_named_user, seed_user,
        seed_user_data, set_item_path, test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_model::entities_media::VirtualFolderInfo;
    use rstest::rstest;

    /// A views request with `includeHidden=true`.
    fn hidden_too() -> UserViewQuery {
        UserViewQuery {
            include_hidden: true,
            ..UserViewQuery::default()
        }
    }

    /// The Live TV view's id is derived from a key the C# strips the
    /// program-data path out of, so two servers with different data directories
    /// agree on it. The key is 12.0's `views/livetv_namedview_livetv` — NOT
    /// the 10.11 `…_namedview_Live TV`, whose value
    /// (`2b2bca16aacc8a14d53a11bb829eafa5`, what a live 10.11.8 serves) is the
    /// stale id the boot consolidation folds onto this one.
    #[test]
    fn live_tv_view_id_is_jellyfin_12s_and_is_data_dir_independent() {
        for data_dir in ["/config", "/data", "/var/lib/ferrofin"] {
            let mode = item_type_lookup::IdDerivation::Jellyfin {
                program_data_path: Some(data_dir.to_owned()),
            };
            let path = std::path::Path::new(data_dir).join("metadata/views/livetv");
            let id = live_tv_view_id_with(&mode, &path)
                .expect("id")
                .simple()
                .to_string();
            assert_eq!(id, "54c4e8dbdc43511d9b3a452c48adf58a", "{data_dir}");
            assert_ne!(id, "2b2bca16aacc8a14d53a11bb829eafa5", "{data_dir}");
            assert_eq!(
                Some(id),
                crate::user_view_repository::named_view_id(
                    &mode,
                    &std::path::Path::new(data_dir).join("metadata"),
                    "livetv"
                )
                .map(|id| id.simple().to_string()),
                "the provisioner and the consolidation agree on the canonical id"
            );
        }
    }

    /// The owner's real 12.0 database carries two `Playlists` views. After the
    /// boot consolidation the user's view list names `Playlists` once.
    #[tokio::test]
    async fn consolidation_leaves_one_playlists_view_in_the_users_views() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x5201);
        seed_user(&db, user_id).await;
        let mode = item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: Some("/config".to_owned()),
        };
        let metadata = std::path::PathBuf::from("/config/metadata");
        let canonical =
            crate::user_view_repository::named_view_id(&mode, &metadata, "playlists").expect("id");
        let stale = item_type_lookup::derive_item_id_with(
            &mode,
            BaseItemKind::UserView,
            "/config/metadata/views/Playlists_namedview_Playlists",
        )
        .expect("id");
        for (id, path) in [
            (canonical, "/config/metadata/views/playlists"),
            (stale, "/config/metadata/views/Playlists"),
        ] {
            seed_item_with_data(
                &db,
                id,
                BaseItemKind::UserView,
                "Playlists",
                r#"{"ViewType":"playlists"}"#,
            )
            .await;
            set_item_path(&db, id, path).await;
        }
        let manager = manager(&db)
            .with_database(db.clone())
            .with_metadata_path(metadata)
            .with_id_derivation(mode);

        let before = manager.get_user_views(user_id).await.expect("views").len();
        assert_eq!(before, 2, "the duplicate is what 12.0 left behind");

        assert_eq!(
            manager
                .consolidate_localized_user_views()
                .await
                .expect("consolidate"),
            1
        );
        let after: Vec<(Uuid, Option<String>)> = manager
            .get_user_views(user_id)
            .await
            .expect("views")
            .into_iter()
            .map(|v| (Uuid::parse_str(&v.id).expect("id"), v.name))
            .collect();
        assert_eq!(after, vec![(canonical, Some("Playlists".to_owned()))]);
    }

    /// `ItemVisibility`'s data directory: a playlist under it is a shared
    /// (`Playlist.IsSharedItem`) one, visible by owner, shares and `OpenAccess`.
    const DATA: &str = "/config/data";

    /// The evaluator the composition root wires into the manager.
    fn visibility(db: &Database) -> Arc<crate::item_visibility::ItemVisibility> {
        Arc::new(crate::item_visibility::ItemVisibility::new(
            db.clone(),
            crate::test_support::item_repository_over(db.clone()),
            Arc::new(crate::localization_manager::LocalizationManager::new("US")),
            DATA.to_owned(),
        ))
    }

    /// A manager wired as the composition root wires it for `GetUserViews`.
    fn manager_with_visibility(
        db: &Database,
        metadata_path: &std::path::Path,
    ) -> FerrofinUserViewManager {
        let users: Arc<dyn UserManager> =
            Arc::new(crate::user_manager::FerrofinUserManager::new(db.clone()));
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        manager(db)
            .with_database(db.clone())
            .with_item_store(persistence)
            .with_metadata_path(metadata_path)
            .with_id_derivation(item_type_lookup::IdDerivation::Jellyfin {
                program_data_path: Some("/config".to_owned()),
            })
            .with_users(users)
            .with_visibility(visibility(db))
    }

    /// Saves one row through the real upsert.
    async fn put(
        db: &Database,
        id: Uuid,
        (kind, name): (BaseItemKind, &str),
        (parent, top_parent): (Option<Uuid>, Uuid),
        path: &str,
        data: &serde_json::Value,
    ) {
        let row = BaseItemEntity {
            id: guid_to_db(id),
            type_: stored_type_name(kind).unwrap_or_default().to_owned(),
            name: Some(name.to_owned()),
            sort_name: Some(name.to_owned()),
            parent_id: parent.map(guid_to_db),
            top_parent_id: Some(guid_to_db(top_parent)),
            path: Some(path.to_owned()),
            is_folder: kinds::is_folder(kind),
            data: Some(data.to_string()),
            ..BaseItemEntity::default()
        };
        FerrofinItemPersistenceService::new(db.clone())
            .save_items(std::slice::from_ref(&row))
            .await
            .expect("save row");
    }

    async fn lists(manager: &FerrofinUserViewManager, user: Uuid, view: &str) -> bool {
        manager
            .get_user_views(user)
            .await
            .expect("views")
            .iter()
            .any(|v| v.name.as_deref() == Some(view))
    }

    async fn block_folders(db: &Database, user: Uuid, folders: &[Uuid]) {
        crate::user_entity_ext::set_preference(
            db,
            &guid_to_db(user),
            ferrofin_db::enums::PreferenceKind::BlockedMediaFolders,
            &folders.iter().map(|id| guid_to_db(*id)).collect::<Vec<_>>(),
        )
        .await
        .expect("block folders");
    }

    /// 12.2 `HasVisibleItem` for the playlists folder: the view is listed when
    /// the user can see a `Playlist` whose `TopParentId` is among their root
    /// folders' top parents — `Playlist.IsVisible` decides, so another user's
    /// private playlist does not count — wherever below its top parent the
    /// playlist sits. The owner's library is the real case for the last part:
    /// every playlist there is a file inside a music album, and 12.1's
    /// direct-child rule served no Playlists view for it.
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one user and folder walked through every case the rule distinguishes"
    )]
    async fn the_playlists_view_needs_a_playlist_the_user_can_see() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x5301);
        crate::test_support::seed_user_with_defaults(&db, user).await;
        let owner = Uuid::from_u128(0x5309);
        let folder = Uuid::from_u128(0x5302);
        put(
            &db,
            folder,
            (BaseItemKind::ManualPlaylistsFolder, "Playlists"),
            (None, folder),
            &format!("{DATA}/playlists"),
            &serde_json::json!({}),
        )
        .await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = manager_with_visibility(&db, tmp.path());
        assert!(
            !lists(&manager, user, "Playlists").await,
            "no playlist: no view"
        );

        let playlist = Uuid::from_u128(0x5303);
        let save_playlist = |data: serde_json::Value| {
            let db = db.clone();
            async move {
                put(
                    &db,
                    playlist,
                    (BaseItemKind::Playlist, "Theirs"),
                    (Some(folder), folder),
                    &format!("{DATA}/playlists/Theirs"),
                    &data,
                )
                .await;
            }
        };
        save_playlist(serde_json::json!({"OwnerUserId": owner, "OpenAccess": false})).await;
        assert!(
            !lists(&manager, user, "Playlists").await,
            "another user's private playlist does not list the view"
        );

        save_playlist(serde_json::json!({
            "OwnerUserId": owner,
            "OpenAccess": false,
            "Shares": [{"UserId": user, "CanEdit": false}],
        }))
        .await;
        assert!(
            lists(&manager, user, "Playlists").await,
            "a playlist shared with the user lists the view"
        );

        save_playlist(serde_json::json!({"OwnerUserId": owner, "OpenAccess": true})).await;
        assert!(
            lists(&manager, user, "Playlists").await,
            "an open-access playlist lists the view"
        );

        // The profile's "Display on home screen" toggle stores the view id in
        // MyMediaExcludes. That hides the home view while leaving the playlist
        // folder and playlist endpoints untouched.
        let playlist_view_id = manager
            .get_user_views(user)
            .await
            .expect("views")
            .into_iter()
            .find(|view| view.name.as_deref() == Some("Playlists"))
            .and_then(|view| Uuid::parse_str(&view.id).ok())
            .expect("derived Playlists view");
        let hide = |ids: Vec<String>| {
            let db = db.clone();
            async move {
                crate::user_entity_ext::set_preference(
                    &db,
                    &guid_to_db(user),
                    ferrofin_db::enums::PreferenceKind::MyMediaExcludes,
                    &ids,
                )
                .await
                .expect("MyMediaExcludes");
            }
        };
        hide(vec![guid_to_db(playlist_view_id)]).await;
        assert!(
            !lists(&manager, user, "Playlists").await,
            "MyMediaExcludes hides the Playlists home view"
        );
        assert!(
            manager
                .get_user_views_with_query(user, &hidden_too())
                .await
                .expect("views including hidden")
                .iter()
                .any(|v| v.name.as_deref() == Some("Playlists")),
            "profile settings can still list the hidden view to re-enable it"
        );
        hide(Vec::new()).await;

        save_playlist(serde_json::json!({"OwnerUserId": owner, "OpenAccess": false})).await;
        assert!(
            !lists(&manager, user, "Playlists").await,
            "private again: the view row the user already has is not listed"
        );

        // A playlist file inside a music album: not a child of the playlists
        // folder, but its top parent is one of the user's.
        let music = Uuid::from_u128(0x5304);
        let album = Uuid::from_u128(0x5305);
        put(
            &db,
            music,
            (BaseItemKind::CollectionFolder, "Music"),
            (None, music),
            "/music",
            &serde_json::json!({"CollectionType": "music"}),
        )
        .await;
        put(
            &db,
            album,
            (BaseItemKind::MusicAlbum, "Album"),
            (Some(music), music),
            "/music/Album",
            &serde_json::json!({}),
        )
        .await;
        put(
            &db,
            Uuid::from_u128(0x5306),
            (BaseItemKind::Playlist, "Album"),
            (Some(album), music),
            "/music/Album/Album.m3u",
            &serde_json::json!({}),
        )
        .await;
        assert!(
            lists(&manager, user, "Playlists").await,
            "a playlist inside a music album lists the view"
        );

        block_folders(&db, user, &[music]).await;
        assert!(
            !lists(&manager, user, "Playlists").await,
            "a library the user may not open is not one of their top parents"
        );
    }

    /// gl-1584: an unscoped query for a user writes their Playlists view, as
    /// upstream's `AddUserToQuery` does through `GetUserViews`
    /// (`LibraryManager.cs:2090-2117`), though `/UserViews` is never asked. The
    /// row is the one `/UserViews` then serves, and a repeat query adds none.
    #[tokio::test]
    async fn an_unscoped_user_query_writes_the_users_playlists_view() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x5401);
        let user_row = crate::test_support::seed_user_with_defaults(&db, user).await;
        let folder = Uuid::from_u128(0x5402);
        put(
            &db,
            folder,
            (BaseItemKind::ManualPlaylistsFolder, "Playlists"),
            (None, folder),
            &format!("{DATA}/playlists"),
            &serde_json::json!({}),
        )
        .await;
        put(
            &db,
            Uuid::from_u128(0x5403),
            (BaseItemKind::Playlist, "Mine"),
            (Some(folder), folder),
            &format!("{DATA}/playlists/Mine"),
            &serde_json::json!({"OwnerUserId": user, "OpenAccess": true}),
        )
        .await;

        // Wired as the composition root wires it: the scope reaches the
        // evaluator and the user-view manager through late slots.
        let scope_user_views = crate::item_repository::UserViewsSlot::default();
        let scope_visibility = crate::item_repository::VisibilitySlot::default();
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        let items = Arc::new(
            FerrofinItemRepository::new(db.clone(), lookup)
                .with_visibility(Arc::clone(&scope_visibility))
                .with_user_views(Arc::clone(&scope_user_views)),
        );
        let evaluator = visibility(&db);
        assert!(scope_visibility.set(Arc::downgrade(&evaluator)).is_ok());
        let tmp = tempfile::tempdir().expect("tempdir");
        let users: Arc<dyn UserManager> =
            Arc::new(crate::user_manager::FerrofinUserManager::new(db.clone()));
        let manager: Arc<dyn UserViewManager> = Arc::new(
            FerrofinUserViewManager::new(Arc::clone(&items) as Arc<dyn ItemRepository>)
                .with_database(db.clone())
                .with_item_store(Arc::new(FerrofinItemPersistenceService::new(db.clone())))
                .with_metadata_path(tmp.path())
                .with_id_derivation(item_type_lookup::IdDerivation::Jellyfin {
                    program_data_path: Some("/config".to_owned()),
                })
                .with_users(users)
                .with_visibility(Arc::clone(&evaluator)),
        );
        scope_user_views.bind(&manager);
        let views = || {
            let items = Arc::clone(&items);
            async move {
                items
                    .get_item_list(&InternalItemsQuery {
                        include_item_types: vec![BaseItemKind::UserView],
                        ..Default::default()
                    })
                    .await
                    .expect("views")
                    .into_iter()
                    .filter(|view| view.name.as_deref() == Some("Playlists"))
                    .map(|view| view.id)
                    .collect::<Vec<_>>()
            }
        };
        assert!(views().await.is_empty(), "no view before any query");

        let mut query = InternalItemsQuery {
            recursive: true,
            ..Default::default()
        };
        query.set_user(Arc::new(user_row));
        items.get_item_list(&query).await.expect("query");
        let written = views().await;
        assert_eq!(
            written.len(),
            1,
            "the user's query wrote their Playlists view"
        );
        let served = manager
            .get_user_views(user)
            .await
            .expect("user views")
            .into_iter()
            .find(|view| view.name.as_deref() == Some("Playlists"))
            .expect("served")
            .id;
        assert_eq!(written, vec![served], "the row /UserViews serves");

        items.get_item_list(&query).await.expect("again");
        assert_eq!(views().await.len(), 1, "a repeat query writes no other row");
    }

    /// gl-1397: `GetNamedView(user, folder.Name, folder.Id, playlists)`
    /// renames a Playlists view whose name is not the folder's
    /// (LibraryManager.cs:3089-3093, v12.4), so the views read after the
    /// playlists folder is renamed lists the new name. A user the folder is
    /// skipped for, with no playlist to see, never reaches `GetNamedView`, so
    /// their row keeps its name.
    #[tokio::test]
    async fn the_playlists_view_follows_a_renamed_playlists_folder() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x5401);
        crate::test_support::seed_user_with_defaults(&db, user).await;
        let folder = Uuid::from_u128(0x5402);
        let name_folder = |name: &'static str| {
            let db = db.clone();
            async move {
                put(
                    &db,
                    folder,
                    (BaseItemKind::ManualPlaylistsFolder, name),
                    (None, folder),
                    &format!("{DATA}/playlists"),
                    &serde_json::json!({}),
                )
                .await;
            }
        };
        let open_playlist = |open: bool| {
            let db = db.clone();
            async move {
                put(
                    &db,
                    Uuid::from_u128(0x5403),
                    (BaseItemKind::Playlist, "Mix"),
                    (Some(folder), folder),
                    &format!("{DATA}/playlists/Mix"),
                    &serde_json::json!({"OwnerUserId": Uuid::from_u128(0x5409), "OpenAccess": open}),
                )
                .await;
            }
        };
        name_folder("Playlists").await;
        open_playlist(true).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = manager_with_visibility(&db, tmp.path());
        let view_names = async || -> Vec<String> {
            manager
                .get_user_views(user)
                .await
                .expect("views")
                .into_iter()
                .filter_map(|v| v.name)
                .collect()
        };
        assert_eq!(view_names().await, ["Playlists"]);

        name_folder("Mixtapes").await;
        assert_eq!(
            view_names().await,
            ["Mixtapes"],
            "the read that renames the row lists the new name"
        );
        let view = manager
            .user_named_view_id(user, folder, "playlists")
            .expect("view id");
        let row = crate::test_support::fetch_item(&db, view).await;
        assert_eq!(
            (row.name.as_deref(), row.sort_name.as_deref()),
            (
                Some("Mixtapes"),
                Some(create_sort_name("Mixtapes").as_str())
            ),
            "the Name setter drops the cached SortName, so the save recomputes it"
        );
        // A rewrite would stamp the sort name back, so a read that matches the
        // name must leave this one alone: no write on the steady-state path.
        let mut marked = row;
        marked.sort_name = Some("SENTINEL".to_owned());
        FerrofinItemPersistenceService::new(db.clone())
            .save_items(std::slice::from_ref(&marked))
            .await
            .expect("mark the row");
        assert_eq!(view_names().await, ["Mixtapes"]);
        assert_eq!(
            crate::test_support::fetch_item(&db, view)
                .await
                .sort_name
                .as_deref(),
            Some("SENTINEL"),
            "a read of an up-to-date name writes nothing"
        );

        open_playlist(false).await;
        name_folder("Lists").await;
        assert!(view_names().await.is_empty(), "no playlist to see: no view");
        let row = crate::test_support::fetch_item(&db, view).await;
        assert_eq!(
            row.name.as_deref(),
            Some("Mixtapes"),
            "a skipped folder never reaches GetNamedView's rename"
        );
    }

    /// A playlists view an adopted blob stores with a numeric `ViewType`
    /// (11, `CollectionType.playlists`) answers to the same rule as the named
    /// one: listed only once the user can see a playlist.
    #[tokio::test]
    async fn a_numeric_playlists_view_type_needs_a_visible_playlist_too() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x5601);
        crate::test_support::seed_user_with_defaults(&db, user).await;
        let folder = Uuid::from_u128(0x5602);
        put(
            &db,
            folder,
            (BaseItemKind::ManualPlaylistsFolder, "Playlists"),
            (None, folder),
            &format!("{DATA}/playlists"),
            &serde_json::json!({}),
        )
        .await;
        let view = Uuid::from_u128(0x5603);
        put(
            &db,
            view,
            (BaseItemKind::UserView, "Adopted"),
            (None, view),
            "/config/metadata/views/adopted",
            &serde_json::json!({"ViewType": 11}),
        )
        .await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = manager_with_visibility(&db, tmp.path());
        assert!(!lists(&manager, user, "Adopted").await, "no playlist");

        put(
            &db,
            Uuid::from_u128(0x5604),
            (BaseItemKind::Playlist, "Open"),
            (Some(folder), folder),
            &format!("{DATA}/playlists/Open"),
            &serde_json::json!({"OpenAccess": true}),
        )
        .await;
        assert!(lists(&manager, user, "Adopted").await, "an open playlist");
    }

    /// The `boxsets` side of 12.2 `HasVisibleItem`: the Collections library is
    /// listed when the user can see a `BoxSet` below it, nested or not, and
    /// `BoxSet.IsVisible` decides — a box set whose movies all sit in a library
    /// the user may not open does not count.
    #[tokio::test]
    async fn the_collections_view_needs_a_box_set_the_user_can_see() {
        use ferrofin_traits::persistence::LinkedChildrenService;
        let db = test_db().await;
        let user = Uuid::from_u128(0x5401);
        crate::test_support::seed_user_with_defaults(&db, user).await;
        let collections = Uuid::from_u128(0x5402);
        let movies = Uuid::from_u128(0x5403);
        let movie = Uuid::from_u128(0x5404);
        let shelf = Uuid::from_u128(0x5405);
        let boxset = Uuid::from_u128(0x5406);
        for (id, kind_name, parents, path, data) in [
            (
                collections,
                (BaseItemKind::CollectionFolder, "Collections"),
                (None, collections),
                format!("{DATA}/collections"),
                serde_json::json!({"CollectionType": "boxsets"}),
            ),
            (
                movies,
                (BaseItemKind::CollectionFolder, "Movies"),
                (None, movies),
                "/movies".to_owned(),
                serde_json::json!({"CollectionType": "movies"}),
            ),
            (
                movie,
                (BaseItemKind::Movie, "Heat"),
                (Some(movies), movies),
                "/movies/Heat.mkv".to_owned(),
                serde_json::json!({}),
            ),
        ] {
            put(&db, id, kind_name, parents, &path, &data).await;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = manager_with_visibility(&db, tmp.path());
        assert!(
            !lists(&manager, user, "Collections").await,
            "no box set: no view"
        );

        // Below a sub-folder, not a direct child of the library.
        put(
            &db,
            shelf,
            (BaseItemKind::Folder, "Shelf"),
            (Some(collections), collections),
            &format!("{DATA}/collections/Shelf"),
            &serde_json::json!({}),
        )
        .await;
        put(
            &db,
            boxset,
            (BaseItemKind::BoxSet, "Heat Collection"),
            (Some(shelf), collections),
            &format!("{DATA}/collections/Shelf/Heat Collection [boxset]"),
            &serde_json::json!({}),
        )
        .await;
        crate::FerrofinLinkedChildrenService::new(db.clone())
            .upsert_linked_child(boxset, movie, 0)
            .await
            .expect("link");
        block_folders(&db, user, &[movies]).await;
        assert!(
            !lists(&manager, user, "Collections").await,
            "a box set of movies the user may not open does not list the view"
        );

        block_folders(&db, user, &[]).await;
        assert!(
            lists(&manager, user, "Collections").await,
            "a visible box set below the library lists the view"
        );
    }

    /// `HasVisibleItem` counts a playlist only when `IsVisible(user)` passes
    /// it, and a private playlist has no administrator exception: an
    /// administrator who neither owns nor shares the only playlist gets no
    /// Playlists view, as Jellyfin 12.2 answers (issue #53). The owner does.
    #[tokio::test]
    async fn an_administrator_gets_no_playlists_view_for_others_private_playlists() {
        let db = test_db().await;
        let data = tempfile::tempdir().expect("tempdir");
        let metadata = tempfile::tempdir().expect("tempdir");
        let (owner, admin) = (Uuid::from_u128(0x5401), Uuid::from_u128(0x5402));
        crate::test_support::seed_named_user(&db, owner, "owner").await;
        crate::test_support::seed_named_user(&db, admin, "admin").await;
        crate::user_entity_ext::set_permission(
            &db,
            &guid_to_db(admin),
            ferrofin_db::enums::PermissionKind::IsAdministrator,
            true,
        )
        .await
        .expect("admin");
        let folder = Uuid::from_u128(0x5403);
        seed_named_item(&db, folder, BaseItemKind::PlaylistsFolder, "Playlists").await;
        let playlist = Uuid::from_u128(0x5404);
        crate::test_support::seed_child_item(&db, playlist, BaseItemKind::Playlist, "Mine", folder)
            .await;
        let mut row = crate::test_support::fetch_item(&db, playlist).await;
        row.path = Some(data.path().join("playlists/Mine").display().to_string());
        // Created playlists sit under the playlists folder, their top parent.
        row.top_parent_id = Some(guid_to_db(folder));
        row.data = Some(
            serde_json::json!({"OwnerUserId": owner, "OpenAccess": false, "Shares": []})
                .to_string(),
        );
        crate::test_support::save_item(&db, &row).await;
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        let visibility = Arc::new(crate::item_visibility::ItemVisibility::new(
            db.clone(),
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
            Arc::new(crate::LocalizationManager::new("US")),
            data.path().to_string_lossy().into_owned(),
        ));
        let manager = manager(&db)
            .with_database(db.clone())
            .with_item_store(Arc::new(FerrofinItemPersistenceService::new(db.clone())))
            .with_metadata_path(metadata.path())
            .with_id_derivation(item_type_lookup::IdDerivation::Jellyfin {
                program_data_path: Some("/config".to_owned()),
            })
            .with_users(Arc::new(crate::user_manager::FerrofinUserManager::new(
                db.clone(),
            )))
            .with_visibility(visibility);
        let lists_playlists = |user| {
            let manager = &manager;
            async move {
                manager
                    .get_user_views(user)
                    .await
                    .expect("views")
                    .iter()
                    .any(|v| v.name.as_deref() == Some("Playlists"))
            }
        };

        assert!(!lists_playlists(admin).await, "an administrator non-member");
        assert!(lists_playlists(owner).await, "the owner");

        // The first child (by SortName) stays hidden; a later open one counts.
        let open = Uuid::from_u128(0x5405);
        crate::test_support::seed_child_item(&db, open, BaseItemKind::Playlist, "Zed", folder)
            .await;
        let mut row = crate::test_support::fetch_item(&db, open).await;
        row.path = Some(data.path().join("playlists/Zed").display().to_string());
        row.top_parent_id = Some(guid_to_db(folder));
        row.data = Some(serde_json::json!({"OwnerUserId": owner, "OpenAccess": true}).to_string());
        crate::test_support::save_item(&db, &row).await;
        assert!(
            lists_playlists(admin).await,
            "an open playlist lists the view"
        );
    }

    fn manager(db: &Database) -> FerrofinUserViewManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinUserViewManager::new(Arc::new(FerrofinItemRepository::new(db.clone(), lookup)))
    }

    /// A manager wired for `get_media_folders` as the composition root wires
    /// it: an item store, plus the user-root store it settles on the way in.
    /// The playlists folder is not its to make: [`provision_at_startup`] does
    /// that, as the server's boot does.
    fn media_folders_manager(
        db: &Database,
        root_path: impl Into<std::path::PathBuf>,
    ) -> FerrofinUserViewManager {
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let root = crate::user_root_folder::UserRootFolderStore::new(
            Arc::clone(&persistence),
            item_type_lookup::IdDerivation::LegacyLowercase,
            root_path,
        );
        manager(db)
            .with_item_store(persistence)
            .with_user_root(root)
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
        ) -> Result<Option<uuid::Uuid>, ServiceError> {
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
        season: Option<Uuid>,
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
                season: None,
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
        fn in_season(mut self, season: Uuid) -> Self {
            self.season = Some(season);
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
                season_id: r.season.map(guid_to_db),
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

    /// `GetUserViews`' last step: the user's saved `OrderedViews` put the views
    /// they name first, in their order — a view by its own id, a user view by the
    /// `DisplayParentId` it stands for — and every view they do not name after
    /// them, in name order. With nothing saved the name order stands, and
    /// `IncludeHidden` lists are ordered the same way.
    #[tokio::test]
    async fn ordered_views_put_the_users_choice_first_and_the_rest_by_name() {
        use ferrofin_db::enums::PreferenceKind;
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x7001);
        seed_user(&db, user_id).await;
        let (a, b, c, d) = (
            Uuid::from_u128(0x7101),
            Uuid::from_u128(0x7102),
            Uuid::from_u128(0x7103),
            Uuid::from_u128(0x7104),
        );
        for (id, name) in [
            (a, "A Movies"),
            (b, "B Shows"),
            (c, "C Music"),
            (d, "D Books"),
        ] {
            seed_named_item(&db, id, BaseItemKind::CollectionFolder, name).await;
        }
        let mgr = manager(&db).with_database(db.clone());
        let names = |views: Vec<BaseItemEntity>| {
            views
                .into_iter()
                .filter_map(|view| view.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(mgr.get_user_views(user_id).await.expect("views")),
            ["A Movies", "B Shows", "C Music", "D Books"],
            "no saved order: by name"
        );

        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user_id),
            PreferenceKind::OrderedViews,
            &[
                guid_to_db(c),
                guid_to_db(Uuid::from_u128(0xDEAD)),
                guid_to_db(a),
            ],
        )
        .await
        .expect("save the order");
        let expected = ["C Music", "A Movies", "B Shows", "D Books"];
        assert_eq!(
            names(mgr.get_user_views(user_id).await.expect("views")),
            expected,
            "named views first in the saved order; an id that is no view is ignored"
        );
        assert_eq!(
            names(
                mgr.get_user_views_with_query(user_id, &hidden_too())
                    .await
                    .expect("views")
            ),
            expected
        );

        // Hidden views are not listed, and the saved order still holds for the rest.
        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user_id),
            PreferenceKind::MyMediaExcludes,
            &[guid_to_db(c)],
        )
        .await
        .expect("hide Music");
        assert_eq!(
            names(mgr.get_user_views(user_id).await.expect("views")),
            ["A Movies", "B Shows", "D Books"]
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

    /// A grouped request whose first typed view is a movies library takes v12's
    /// movies arm (`UserViewManager.cs:419-423`, `GetLatestMovieItems`): the
    /// newest `limit` rows of every view, one per presentation key, then
    /// grouped. Two new episodes of one series fill a `limit=2` page, which
    /// then shows ONE card; the plain path read `limit * 5` rows and filled
    /// both cards (the series, then the movie).
    #[tokio::test]
    async fn latest_with_a_movies_first_view_reads_limit_rows_then_groups() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let series = Uuid::from_u128(0x251);
        let (movie, e1, e2) = (
            Uuid::from_u128(0x351),
            Uuid::from_u128(0x352),
            Uuid::from_u128(0x353),
        );
        seed(
            &db,
            &[
                Row::new(series, BaseItemKind::Series, "Series 01", day(1), SHOWS),
                Row::new(movie, BaseItemKind::Movie, "Movie 0001", day(3), MOVIES),
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

        let query = |group_items| LatestItemsQuery {
            limit: Some(2),
            group_items,
            ..LatestItemsQuery::default()
        };
        assert_eq!(
            shape(&latest(&mgr, query(true)).await),
            vec![(Some(series), vec![e2, e1])]
        );
        // Ungrouped requests never short-circuit: the plain path, two rows.
        assert_eq!(
            shape(&latest(&mgr, query(false)).await),
            vec![(None, vec![e2]), (None, vec![e1])]
        );
    }

    /// One series with ten new episodes beside two older movies, all in one
    /// library; the episodes are in no season, so the series' card is its
    /// newest episode.
    async fn seed_series_burst(db: &Database, library: Uuid) -> (Uuid, Vec<Uuid>, [Uuid; 2]) {
        let series = Uuid::from_u128(0x261);
        let movies = [Uuid::from_u128(0x361), Uuid::from_u128(0x362)];
        let episodes: Vec<Uuid> = (1..=10).map(|n| Uuid::from_u128(0x370 + n)).collect();
        let names: Vec<String> = (1..=10).map(|n| format!("S1E{n:02}")).collect();
        let mut rows = vec![
            Row::new(series, BaseItemKind::Series, "Series 01", day(1), library),
            Row::new(
                movies[0],
                BaseItemKind::Movie,
                "Movie 0001",
                day(2),
                library,
            ),
            Row::new(
                movies[1],
                BaseItemKind::Movie,
                "Movie 0002",
                day(3),
                library,
            ),
        ];
        rows.extend(
            episodes
                .iter()
                .zip(&names)
                .zip(11..)
                .map(|((id, name), d)| {
                    Row::new(*id, BaseItemKind::Episode, name, day(d), library)
                        .under(series)
                        .of_series(series, "Series 01")
                }),
        );
        seed(db, &rows).await;
        (series, episodes, movies)
    }

    /// A grouped request whose parents carry no collection type (a mixed
    /// library, scoped, or unscoped with no typed root folder: here no
    /// playlists folder, which every real server has and whose `playlists`
    /// type would send the unscoped request down the plain path, as
    /// upstream) takes v12's `unknown` arm
    /// (`UserViewManager.cs:425-429`, `Querying.cs:134-144`): the newest movies
    /// and the newest TV cards, merged by date added. The plain path read
    /// `limit * 5` rows, all ten of them the series' new episodes, and showed
    /// one card (jellyfin/jellyfin#17672).
    #[rstest::rstest]
    #[case::scoped(true)]
    #[case::unscoped(false)]
    #[tokio::test]
    async fn latest_for_a_mixed_library_merges_movies_and_shows(#[case] scoped: bool) {
        let db = test_db().await;
        let mixed = Uuid::from_u128(0x105);
        seed_named_item(&db, mixed, BaseItemKind::CollectionFolder, "Mixed").await;
        let (series, episodes, movies) = seed_series_burst(&db, mixed).await;
        let mgr = manager_with_folders(&db, vec![(mixed, None)]);

        let query = LatestItemsQuery {
            parent_id: scoped.then_some(mixed),
            limit: Some(2),
            ..LatestItemsQuery::default()
        };
        assert_eq!(
            shape(&latest(&mgr, query).await),
            vec![(Some(series), vec![episodes[9]]), (None, vec![movies[1]])]
        );
    }

    /// The `unknown` arm merges by each row's OWN `DateCreated`
    /// (`Querying.cs:141`, `dto.DateCreated`), not by the series' newest
    /// episode that orders the TV arm: three new episodes of a one-season
    /// series make a Series card, which keeps the series' old date and so
    /// sorts behind an older movie. Equal dates break on `Id DESC`: the card
    /// (a larger id) before a movie of the same day, though the movies half
    /// comes first.
    #[tokio::test]
    async fn latest_unknown_arm_orders_a_series_card_by_its_own_date() {
        let db = test_db().await;
        let mixed = Uuid::from_u128(0x105);
        seed_named_item(&db, mixed, BaseItemKind::CollectionFolder, "Mixed").await;
        let (series, season, movie, same_day) = (
            Uuid::from_u128(0x281),
            Uuid::from_u128(0x282),
            Uuid::from_u128(0x283),
            Uuid::from_u128(0x280),
        );
        let episodes = [
            Uuid::from_u128(0x284),
            Uuid::from_u128(0x285),
            Uuid::from_u128(0x286),
        ];
        let mut rows = vec![
            Row::new(series, BaseItemKind::Series, "Series 02", day(1), mixed),
            Row::new(season, BaseItemKind::Season, "Season 1", day(1), mixed)
                .under(series)
                .of_series(series, "Series 02"),
            Row::new(movie, BaseItemKind::Movie, "Movie 0003", day(10), mixed),
            Row::new(same_day, BaseItemKind::Movie, "Movie 0004", day(1), mixed),
        ];
        rows.extend(episodes.iter().map(|id| {
            Row::new(*id, BaseItemKind::Episode, "S1E", day(20), mixed)
                .under(season)
                .of_series(series, "Series 02")
                .in_season(season)
        }));
        seed(&db, &rows).await;
        let mgr = manager_with_folders(&db, vec![(mixed, None)]);

        let query = LatestItemsQuery {
            parent_id: Some(mixed),
            limit: Some(3),
            ..LatestItemsQuery::default()
        };
        assert_eq!(
            shape(&latest(&mgr, query).await),
            vec![
                (None, vec![movie]),
                (None, vec![series]),
                (None, vec![same_day]),
            ]
        );
    }

    /// A parent that is neither a library nor a view (here a series) has no
    /// collection type either, so upstream's `collectionType is null` sends it
    /// through the `unknown` arm too: the series' one card, its newest
    /// episode, where the plain path grouped every new episode under it.
    #[tokio::test]
    async fn latest_under_a_series_takes_the_unknown_arm() {
        let db = test_db().await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "Shows").await;
        let (series, episodes, _) = seed_series_burst(&db, SHOWS).await;
        // A series parent scopes through the ancestor closure.
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        for episode in &episodes {
            persistence
                .set_ancestors(*episode, &[series, SHOWS])
                .await
                .expect("ancestors");
        }
        let mgr = manager_with_folders(&db, vec![(SHOWS, Some(CollectionTypeOptions::tvshows))]);

        let query = LatestItemsQuery {
            parent_id: Some(series),
            limit: Some(2),
            ..LatestItemsQuery::default()
        };
        assert_eq!(
            shape(&latest(&mgr, query).await),
            vec![(Some(series), vec![episodes[9]])]
        );
    }

    /// Equal `DateCreated` (hard-linked fixtures share one btime) breaks on
    /// `SortName DESC` on the plain path — Jellyfin's `0500, 0499, …`, not the
    /// old `ASC`. Ungrouped, so no arm of `GetLatestItemList` (which break ties
    /// on `Id DESC`) takes the request; the names sort the opposite way to the
    /// ids, so an `Id` tie-break would fail.
    #[tokio::test]
    async fn latest_breaks_date_created_ties_by_sort_name_desc() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        let ids: Vec<Uuid> = (1..=5).map(|n| Uuid::from_u128(0x400 + n)).collect();
        let names: Vec<String> = (1..=5).map(|n| format!("Movie {:04}", 6 - n)).collect();
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
                group_items: false,
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&groups),
            vec![
                (None, vec![ids[0]]),
                (None, vec![ids[1]]),
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
    ///
    /// The libraries are typed `musicvideos`, a type with no arm of its own:
    /// untyped ones would take the `unknown` arm, which never reaches the
    /// plain path's over-fetch.
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
        let mgr = manager_with_folders(
            &db,
            vec![
                (MOVIES, Some(CollectionTypeOptions::musicvideos)),
                (SHOWS, Some(CollectionTypeOptions::musicvideos)),
            ],
        );

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
        // No virtual-folder manager, so the parent has no collection type and
        // the request takes the `unknown` arm, whose rows (here every track
        // and the music video, all from its movies half) still resolve their
        // index containers. A music-typed parent short-circuits into v12's
        // album query instead (`latest_parent_music_library_returns_newest_albums`),
        // which returns album rows and never groups anything.
        let mgr = manager(&db);

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
    }

    /// v12's music branch (`UserViewManager.cs:388-392` →
    /// `BaseItemRepository.Querying.cs:147-180`): a music library answers with
    /// the newest ALBUMS, ordered by the albums' own `DateCreated`, and `limit`
    /// caps albums. The tracks' dates run the other way here, which is exactly
    /// the answer 10.11.8 gave and v12 changed: nothing about a track can put
    /// its album on the list, or keep it off.
    ///
    /// `GroupItems = false` still takes the generic path, where the library's
    /// media type (`music` → `Audio`) applies and the music video is not
    /// "latest music".
    #[tokio::test]
    async fn latest_parent_music_library_returns_newest_albums() {
        let db = test_db().await;
        seed_named_item(&db, MUSIC, BaseItemKind::CollectionFolder, "Music").await;
        let (old_album, new_album) = (Uuid::from_u128(0x751), Uuid::from_u128(0x752));
        let (t_new, t_old, mv) = (
            Uuid::from_u128(0x761),
            Uuid::from_u128(0x762),
            Uuid::from_u128(0x763),
        );
        seed(
            &db,
            &[
                Row::new(old_album, BaseItemKind::MusicAlbum, "Older", day(3), MUSIC),
                Row::new(new_album, BaseItemKind::MusicAlbum, "Newer", day(6), MUSIC),
                // The newest TRACK sits in the OLDEST album.
                Row::new(t_new, BaseItemKind::Audio, "T new", day(9), MUSIC)
                    .under(old_album)
                    .on_album("Older"),
                Row::new(t_old, BaseItemKind::Audio, "T old", day(1), MUSIC)
                    .under(new_album)
                    .on_album("Newer"),
                Row::new(mv, BaseItemKind::MusicVideo, "MV", day(8), MUSIC).under(new_album),
            ],
        )
        .await;
        let mgr = manager_with_folders(&db, vec![(MUSIC, Some(CollectionTypeOptions::music))]);

        let albums = latest(
            &mgr,
            LatestItemsQuery {
                parent_id: Some(MUSIC),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&albums),
            vec![(None, vec![new_album]), (None, vec![old_album])],
            "the newest albums, ungrouped — an album has no index container"
        );

        // A limit of 1 yields one album. This is a smoke test, not the pin for
        // "the SQL limit caps ALBUMS and is the caller's, un-over-fetched":
        // `get_latest_items` breaks at `list.len() >= limit` regardless, so it
        // would pass on a broken SQL limit too. The real pin is
        // `item_repository::tests::latest_item_list_returns_newest_albums_for_music`,
        // which reads the rows straight off the repository.
        let one = latest(
            &mgr,
            LatestItemsQuery {
                parent_id: Some(MUSIC),
                limit: Some(1),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(shape(&one), vec![(None, vec![new_album])]);

        // Ungrouped, the generic path answers with the library's media type.
        let tracks = latest(
            &mgr,
            LatestItemsQuery {
                parent_id: Some(MUSIC),
                group_items: false,
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&tracks),
            vec![(None, vec![t_new]), (None, vec![t_old])],
            "Audio only — the music video is not latest music"
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
        // No virtual-folder manager: a type-less parent takes the `unknown`
        // arm (`includeItemTypes` lifts its Video media-type default), whose
        // movies half reads the track, so this is the container resolution of
        // a row no album query returned.
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

    /// The music branch fires on the parents' collection type and nothing else,
    /// so a request scoped to a movies parent takes the movies arm even on a
    /// server that does have a music library — and a stray `MusicAlbum` row
    /// sitting in a movies library is a folder, which the movies arm's filter
    /// (`IsFolder = false`) leaves out. The other half: a music library with
    /// no albums answers empty rather than widening its top-parent scope.
    #[tokio::test]
    async fn latest_music_branch_needs_a_music_parent() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, MUSIC, BaseItemKind::CollectionFolder, "Music").await;
        let (movie, stray_album) = (Uuid::from_u128(0x771), Uuid::from_u128(0x772));
        seed(
            &db,
            &[
                Row::new(movie, BaseItemKind::Movie, "Film", day(2), MOVIES),
                Row::new(
                    stray_album,
                    BaseItemKind::MusicAlbum,
                    "Stray",
                    day(9),
                    MOVIES,
                ),
            ],
        )
        .await;
        let mgr = manager_with_folders(
            &db,
            vec![
                (MOVIES, Some(CollectionTypeOptions::movies)),
                (MUSIC, Some(CollectionTypeOptions::music)),
            ],
        );

        let movies = latest(
            &mgr,
            LatestItemsQuery {
                parent_id: Some(MOVIES),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(
            shape(&movies),
            vec![(None, vec![movie])],
            "a movies parent takes the movies arm, folders excluded"
        );

        let empty = latest(
            &mgr,
            LatestItemsQuery {
                parent_id: Some(MUSIC),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert!(
            empty.is_empty(),
            "a music library with no albums answers empty: {empty:?}",
            empty = shape(&empty)
        );
    }

    /// `isPlayed` is a SQL predicate (it needs the user), not a post-filter
    /// over an already-cut page: SIX played movies are newer than the one
    /// unplayed movie, and the plain path (ungrouped) over-fetches
    /// `limit * 5 = 5` rows while the movies arm (grouped, Movies the first
    /// view) reads `limit = 1`, so a post-filter over either page would return
    /// nothing while the SQL predicate finds the fresh one. A music parent
    /// drops the filter entirely.
    #[tokio::test]
    async fn latest_pushes_is_played_into_sql() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, MUSIC, BaseItemKind::CollectionFolder, "Music").await;
        let user = crate::test_support::seed_user_with_defaults(&db, Uuid::from_u128(0x9)).await;
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
        )
        .with_visibility(visibility(&db));

        for group_items in [false, true] {
            let unplayed = latest(
                &mgr,
                LatestItemsQuery {
                    user: Some(Arc::new(user.clone())),
                    is_played: Some(false),
                    group_items,
                    limit: Some(1),
                    ..LatestItemsQuery::default()
                },
            )
            .await;
            // The played movies are out; the played TRACK is out too — the
            // views fallback keeps `isPlayed` (the music exemption is decided
            // on the explicit parent only, as upstream).
            assert_eq!(
                shape(&unplayed),
                vec![(None, vec![fresh])],
                "group_items={group_items}"
            );
        }

        // `GroupItems = false` keeps a music parent on the generic path, which
        // is where `isPlayed` would still be a predicate — and it is not:
        // `latest_parents` cleared it on the explicit music parent, so the
        // played track comes back. (With grouping on, v12's album query never
        // reads a track's played state at all.)
        let music = latest(
            &mgr,
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(MUSIC),
                is_played: Some(false),
                group_items: false,
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

    /// `parentId` = a tvshows library takes v12's `GetLatestTvShowItems`, and
    /// this is only provable where the plain path would answer differently:
    /// series A has TWELVE same-day episodes — more than the plain path's
    /// `limit * 5 = 10` row cap — so the plain query never reaches series B
    /// at all, while the TV path (limit caps SERIES) answers one card for each
    /// of the two newest series. Their episodes carry no season, so each card
    /// is the series' newest episode (same day: the highest id), grouped under
    /// its series; series C (day 3) is not among the newest two.
    #[tokio::test]
    async fn latest_parent_tvshows_library_answers_one_card_per_series() {
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
        // Same-day ties break on `Id DESC`: A's newest is its last episode.
        assert_eq!(
            shape(&groups),
            vec![(Some(sa), vec![a_eps[11]]), (Some(sb), vec![b_new])]
        );

        // A type-less parent (no virtual-folder manager) takes the `unknown`
        // arm, whose shows half is this same answer (there are no movies).
        let untyped = latest(
            &manager(&db),
            LatestItemsQuery {
                parent_id: Some(SHOWS),
                limit: Some(2),
                ..LatestItemsQuery::default()
            },
        )
        .await;
        assert_eq!(shape(&untyped), shape(&groups));

        // A parent of a type with no arm (`musicvideos`) takes the plain path
        // and stops at the 10-row over-fetch — all of it series A — which is
        // what the grouped query exists to fix.
        let plain = latest(
            &manager_with_folders(&db, vec![(SHOWS, Some(CollectionTypeOptions::musicvideos))]),
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

    #[tokio::test]
    async fn hidden_home_library_still_supplies_latest_items() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0xAA19);
        let user = crate::test_support::seed_user_with_defaults(&db, user_id).await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        let movie = Uuid::from_u128(0xAA20);
        seed(
            &db,
            &[Row::new(
                movie,
                BaseItemKind::Movie,
                "Movie",
                day(9),
                MOVIES,
            )],
        )
        .await;
        let mgr = manager(&db)
            .with_database(db.clone())
            .with_visibility(visibility(&db));
        let mut query = LatestItemsQuery {
            user: Some(Arc::new(user.clone())),
            ..Default::default()
        };
        let expected = vec![(None, vec![movie])];
        assert_eq!(shape(&latest(&mgr, query.clone()).await), expected);

        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user_id),
            ferrofin_db::enums::PreferenceKind::MyMediaExcludes,
            &[guid_to_db(MOVIES)],
        )
        .await
        .expect("hide home tile");
        let user_views = mgr.get_user_views(user_id).await.expect("home views");
        assert!(user_views.is_empty(), "{user_views:?}");
        assert_eq!(
            mgr.get_user_root_children(&user)
                .await
                .expect("root children")
                .len(),
            1
        );
        assert_eq!(
            shape(&latest(&mgr, query.clone()).await),
            expected,
            "MyMediaExcludes must not suppress latest items"
        );

        query.latest_item_excludes = vec![MOVIES];
        assert!(
            latest(&mgr, query.clone()).await.is_empty(),
            "LatestItemExcludes still applies"
        );
        query.parent_id = Some(MOVIES);
        assert_eq!(
            shape(&latest(&mgr, query).await),
            expected,
            "an explicit parent remains browsable regardless of home preferences"
        );
    }

    /// `GetChildren(user, true)` takes the virtual children too, and
    /// `Folder.IsVisible` exempts a plugin folder from the library rules.
    #[tokio::test]
    async fn a_blocked_library_leaves_the_user_root_children_and_the_playlists_folder_stays() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0xAA22);
        let user = crate::test_support::seed_user_with_defaults(&db, user_id).await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        let playlists = Uuid::from_u128(0xAA23);
        seed_named_item(&db, playlists, BaseItemKind::PlaylistsFolder, "Playlists").await;
        seed_named_item(&db, MUSIC, BaseItemKind::CollectionFolder, "Music").await;
        block_folders(&db, user_id, &[MOVIES]).await;
        let mgr = manager(&db).with_visibility(visibility(&db));
        let children = mgr.get_user_root_children(&user).await.expect("children");
        let names: Vec<_> = children
            .iter()
            .filter_map(|row| row.name.as_deref())
            .collect();
        assert_eq!(names, ["Music", "Playlists"]);
    }

    /// `UserRootFolder.GetEligibleChildrenForRecursiveChildren` appends
    /// `RootFolder.VirtualChildren` to the libraries, which `GetChildren`
    /// keeps in `SortName` order. `LibraryController.GetMediaFolders` sorts the
    /// concatenation instead, so there the folder stays among the libraries.
    /// Both spellings: `root_children` reads the one an older Ferrofin wrote too.
    #[rstest]
    #[case(BaseItemKind::PlaylistsFolder)]
    #[case(BaseItemKind::ManualPlaylistsFolder)]
    #[tokio::test]
    async fn the_user_root_children_list_the_playlists_folder_after_the_libraries(
        #[case] kind: BaseItemKind,
    ) {
        let db = test_db().await;
        let user = crate::test_support::seed_user_with_defaults(&db, Uuid::from_u128(0xAA24)).await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        let playlists = Uuid::from_u128(0xAA25);
        seed_named_item(&db, playlists, kind, "Playlists").await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "TV Shows").await;
        let mgr = manager(&db).with_visibility(visibility(&db));
        let names = |rows: Vec<BaseItemEntity>| {
            rows.into_iter()
                .filter_map(|row| row.name)
                .collect::<Vec<String>>()
        };

        assert_eq!(
            names(mgr.get_user_root_children(&user).await.expect("children")),
            ["Movies", "TV Shows", "Playlists"]
        );
        assert_eq!(
            names(mgr.get_media_folders().await.expect("folders")),
            ["Movies", "Playlists", "TV Shows"]
        );
    }

    /// `GetMediaFolders` sorts `Children.Concat(VirtualChildren)` with the
    /// stable `OrderBy(i => i.SortName)`, the request culture's
    /// `string.CompareTo`: `~`, which the sort name keeps, sorts before the
    /// letters, where an ordinal comparison puts it after them; and a library
    /// whose sort name ties with the playlists folder's stays ahead of it.
    #[tokio::test]
    async fn the_media_folders_sort_in_the_request_culture() {
        let db = test_db().await;
        let playlists = Uuid::from_u128(0xAA26);
        seed_named_item(&db, playlists, BaseItemKind::PlaylistsFolder, "Playlists").await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Films").await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "~Archive").await;
        let library = Uuid::from_u128(0xAA27);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Playlists").await;
        let mgr = manager(&db).with_visibility(visibility(&db));

        let folders =
            ferrofin_util::current_culture::scope("en-US".to_owned(), mgr.get_media_folders())
                .await
                .expect("folders");
        let listed: Vec<_> = folders
            .iter()
            .map(|row| {
                (
                    row.name.as_deref().unwrap_or_default(),
                    item_type_lookup::kind_from_type_name(&row.type_),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("~Archive", Some(BaseItemKind::CollectionFolder)),
                ("Films", Some(BaseItemKind::CollectionFolder)),
                ("Playlists", Some(BaseItemKind::CollectionFolder)),
                ("Playlists", Some(BaseItemKind::PlaylistsFolder)),
            ]
        );
    }

    /// The first typed parent decides an unscoped grouped request, and the
    /// playlists folder's type matches no arm of `GetItemsForLatestItems`. So
    /// it must not come before a tvshows library that sorts after it: series A
    /// has twelve newer episodes, the plain query's `limit * 5 = 10` rows are
    /// all A's, and only the tvshows arm reaches B.
    #[tokio::test]
    async fn an_unscoped_latest_takes_the_arm_of_a_library_sorting_after_playlists() {
        let db = test_db().await;
        let user = crate::test_support::seed_user_with_defaults(&db, Uuid::from_u128(0xAA26)).await;
        seed_named_item(&db, SHOWS, BaseItemKind::CollectionFolder, "TV Shows").await;
        let playlists = Uuid::from_u128(0xAA27);
        seed_named_item(&db, playlists, BaseItemKind::PlaylistsFolder, "Playlists").await;
        let (sa, sb) = (Uuid::from_u128(0xAA31), Uuid::from_u128(0xAA32));
        let b_new = Uuid::from_u128(0xAA33);
        let a_eps: Vec<Uuid> = (1..=12).map(|n| Uuid::from_u128(0xAA40 + n)).collect();
        let a_names: Vec<String> = (1..=12).map(|n| format!("A e{n:02}")).collect();
        let mut rows = vec![
            Row::new(sa, BaseItemKind::Series, "A", day(1), SHOWS),
            Row::new(sb, BaseItemKind::Series, "B", day(1), SHOWS),
            Row::new(b_new, BaseItemKind::Episode, "B e1", day(8), SHOWS)
                .under(sb)
                .of_series(sb, "B"),
        ];
        for (id, name) in a_eps.iter().zip(&a_names) {
            rows.push(
                Row::new(*id, BaseItemKind::Episode, name, day(9), SHOWS)
                    .under(sa)
                    .of_series(sa, "A"),
            );
        }
        seed(&db, &rows).await;
        let mgr = manager_with_folders(&db, vec![(SHOWS, Some(CollectionTypeOptions::tvshows))])
            .with_visibility(visibility(&db));

        let groups = latest(
            &mgr,
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                limit: Some(2),
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert_eq!(
            shape(&groups),
            vec![(Some(sa), vec![a_eps[11]]), (Some(sb), vec![b_new])]
        );
    }

    /// `user`'s grouped `view_type` view over `libraries`, as `GetUserViews`
    /// builds one: a `UserView` with a `ViewType`, no parent and no
    /// `DisplayParentId`, and the libraries in the user's `GroupedFolders`.
    async fn seed_grouped_view(
        db: &Database,
        user: Uuid,
        view: Uuid,
        view_type: &str,
        libraries: &[(Uuid, &str, Option<&str>)],
    ) -> UserEntity {
        let entity = crate::test_support::seed_user_with_defaults(db, user).await;
        for (id, name, collection_type) in libraries {
            seed_library(db, *id, name, *collection_type).await;
        }
        let ids: Vec<Uuid> = libraries.iter().map(|(id, _, _)| *id).collect();
        group(db, user, &ids).await;
        seed_item_with_data(
            db,
            view,
            BaseItemKind::UserView,
            "Grouped",
            &format!(r#"{{"ViewType":"{view_type}"}}"#),
        )
        .await;
        entity
    }

    /// A grouped view stands for every library in it
    /// (`SetTopParentIdsOrAncestors`, `GetTopParentIdsForQuery`'s grouping arm:
    /// an untyped library joins, an ungrouped one stays out), and every view
    /// parent being a movies view narrows to `Movie` (`GetItemsForLatestItems`:
    /// "movies showing up in tv, etc."), so neither the episode nor the music
    /// video of the untyped library is listed. Grouped or not, the row is the
    /// same three movies.
    #[rstest]
    #[case(true)]
    #[case(false)]
    #[tokio::test]
    async fn a_grouped_movies_view_lists_the_movies_of_every_library_in_it(
        #[case] group_items: bool,
    ) {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xBB00 + n);
        let (view, action, comedy, mixed, elsewhere) = (id(1), id(2), id(3), id(4), id(5));
        let user = seed_grouped_view(
            &db,
            id(6),
            view,
            "movies",
            &[
                (action, "Action", Some("movies")),
                (comedy, "Comedy", Some("movies")),
                (mixed, "Mixed", Some("mixed")),
            ],
        )
        .await;
        seed_library(&db, elsewhere, "Elsewhere", Some("movies")).await;
        let (first, second, third) = (id(11), id(12), id(13));
        seed(
            &db,
            &[
                Row::new(first, BaseItemKind::Movie, "First", day(3), action),
                Row::new(second, BaseItemKind::Movie, "Second", day(4), comedy),
                Row::new(third, BaseItemKind::Movie, "Third", day(5), mixed),
                Row::new(id(14), BaseItemKind::MusicVideo, "Clip", day(7), mixed),
                Row::new(id(15), BaseItemKind::Episode, "Pilot", day(8), mixed),
                Row::new(id(16), BaseItemKind::Movie, "Outside", day(9), elsewhere),
            ],
        )
        .await;

        let groups = latest(
            &manager(&db),
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(view),
                limit: Some(20),
                group_items,
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert_eq!(
            shape(&groups),
            vec![
                (None, vec![third]),
                (None, vec![second]),
                (None, vec![first])
            ]
        );
    }

    /// `userView.CollectionType` decides a grouped request, so a movies view
    /// takes the movies arm (`GetLatestMovieItems`), whose ties break on `Id
    /// DESC`. The plain query an ungrouped request takes breaks them on
    /// `SortName DESC`, which here is the other way round.
    #[tokio::test]
    async fn a_grouped_movies_view_takes_the_movies_arm() {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xC200 + n);
        let (view, action, comedy) = (id(1), id(2), id(3));
        let user = seed_grouped_view(
            &db,
            id(4),
            view,
            "movies",
            &[
                (action, "Action", Some("movies")),
                (comedy, "Comedy", Some("movies")),
            ],
        )
        .await;
        let (low_id, high_id) = (id(11), id(12));
        seed(
            &db,
            &[
                Row::new(low_id, BaseItemKind::Movie, "Z", day(3), action),
                Row::new(high_id, BaseItemKind::Movie, "A", day(3), comedy),
            ],
        )
        .await;
        let mgr = manager(&db);
        let query = |group_items| LatestItemsQuery {
            user: Some(Arc::new(user.clone())),
            parent_id: Some(view),
            limit: Some(20),
            group_items,
            ..LatestItemsQuery::default()
        };

        assert_eq!(
            shape(&latest(&mgr, query(true)).await),
            vec![(None, vec![high_id]), (None, vec![low_id])]
        );
        assert_eq!(
            shape(&latest(&mgr, query(false)).await),
            vec![(None, vec![low_id]), (None, vec![high_id])]
        );
    }

    /// `userView.CollectionType` decides a grouped request too, so a TV view
    /// takes the tvshows arm across its libraries: series A has twelve newer
    /// episodes in one library, the plain query's `limit * 5 = 10` rows would
    /// all be A's, and only that arm reaches B in the other. An ungrouped
    /// request reads rows, narrowed to `Episode`, so the untyped library's
    /// movie stays out of both.
    #[tokio::test]
    async fn a_grouped_tv_view_answers_one_card_per_series_across_its_libraries() {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xBC00 + n);
        let (view, anime, drama, mixed, elsewhere) = (id(1), id(2), id(3), id(4), id(5));
        let user = seed_grouped_view(
            &db,
            id(6),
            view,
            "tvshows",
            &[
                (anime, "Anime", Some("tvshows")),
                (drama, "Drama", Some("tvshows")),
                (mixed, "Mixed", Some("mixed")),
            ],
        )
        .await;
        seed_library(&db, elsewhere, "Elsewhere", Some("tvshows")).await;
        let (sa, sb, sc) = (id(11), id(12), id(13));
        let (b_new, c_new, movie) = (id(14), id(15), id(16));
        let a_eps: Vec<Uuid> = (1..=12).map(|n| id(0x20 + n)).collect();
        let a_names: Vec<String> = (1..=12).map(|n| format!("A e{n:02}")).collect();
        let mut rows = vec![
            Row::new(sa, BaseItemKind::Series, "A", day(1), anime),
            Row::new(sb, BaseItemKind::Series, "B", day(1), drama),
            Row::new(sc, BaseItemKind::Series, "C", day(1), elsewhere),
            Row::new(b_new, BaseItemKind::Episode, "B e1", day(8), drama)
                .under(sb)
                .of_series(sb, "B"),
            Row::new(c_new, BaseItemKind::Episode, "C e1", day(10), elsewhere)
                .under(sc)
                .of_series(sc, "C"),
            Row::new(movie, BaseItemKind::Movie, "Film", day(10), mixed),
        ];
        for (episode, name) in a_eps.iter().zip(&a_names) {
            rows.push(
                Row::new(*episode, BaseItemKind::Episode, name, day(9), anime)
                    .under(sa)
                    .of_series(sa, "A"),
            );
        }
        seed(&db, &rows).await;
        let mgr = manager(&db);
        let query = |limit, group_items| LatestItemsQuery {
            user: Some(Arc::new(user.clone())),
            parent_id: Some(view),
            limit: Some(limit),
            group_items,
            ..LatestItemsQuery::default()
        };

        assert_eq!(
            shape(&latest(&mgr, query(2, true)).await),
            vec![(Some(sa), vec![a_eps[11]]), (Some(sb), vec![b_new])]
        );

        let rows: Vec<Uuid> = latest(&mgr, query(20, false))
            .await
            .iter()
            .flat_map(|(_, items)| items.iter().map(uuid))
            .collect();
        assert_eq!(rows.len(), 13, "{rows:?}");
        assert_eq!(rows.last(), Some(&b_new));
    }

    /// `GetChildren(user, true)` in the grouping arm: a library the user may
    /// not open leaves the view, whatever the preference still names.
    #[tokio::test]
    async fn a_grouped_view_never_reaches_a_library_the_user_may_not_open() {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xBD00 + n);
        let (view, open, closed, user_id) = (id(1), id(2), id(3), id(4));
        let user = seed_grouped_view(
            &db,
            user_id,
            view,
            "movies",
            &[
                (open, "Open", Some("movies")),
                (closed, "Closed", Some("movies")),
            ],
        )
        .await;
        block_folders(&db, user_id, &[closed]).await;
        let allowed = id(11);
        seed(
            &db,
            &[
                Row::new(allowed, BaseItemKind::Movie, "Allowed", day(3), open),
                Row::new(id(12), BaseItemKind::Movie, "Hidden", day(4), closed),
            ],
        )
        .await;

        let groups = latest(
            &manager(&db),
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(view),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert_eq!(shape(&groups), vec![(None, vec![allowed])]);
    }

    /// A view nothing is grouped into stands for no library: upstream scopes
    /// to the view itself rather than widen to every library ("Prevent
    /// searching in all libraries due to empty filter").
    #[tokio::test]
    async fn a_view_grouping_no_library_lists_nothing() {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xBE00 + n);
        let (view, library, user_id) = (id(1), id(2), id(3));
        let user = seed_grouped_view(
            &db,
            user_id,
            view,
            "movies",
            &[(library, "Movies", Some("movies"))],
        )
        .await;
        group(&db, user_id, &[]).await;
        seed(
            &db,
            &[Row::new(id(11), BaseItemKind::Movie, "M", day(3), library)],
        )
        .await;

        let groups = latest(
            &manager(&db),
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(view),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert!(groups.is_empty(), "{:?}", shape(&groups));
    }

    /// A view of one library follows its `DisplayParentId` to that library,
    /// and on an adopted database to the physical folder its rows carry as
    /// their `TopParentId`.
    #[tokio::test]
    async fn a_view_of_one_library_lists_that_library() {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xBF00 + n);
        let (view, library, physical) = (id(1), id(2), id(3));
        let user = crate::test_support::seed_user_with_defaults(&db, id(4)).await;
        seed_item_with_data(
            &db,
            library,
            BaseItemKind::CollectionFolder,
            "Movies",
            &format!(
                r#"{{"CollectionType":"movies","PhysicalFolderIds":["{}"]}}"#,
                physical.simple()
            ),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "Movies").await;
        seed_item_with_data(
            &db,
            view,
            BaseItemKind::UserView,
            "Movies",
            &format!(
                r#"{{"ViewType":"movies","DisplayParentId":"{}"}}"#,
                library.simple()
            ),
        )
        .await;
        let movie = id(11);
        seed(
            &db,
            &[Row::new(movie, BaseItemKind::Movie, "M", day(3), physical)],
        )
        .await;

        let groups = latest(
            &manager(&db),
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(view),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert_eq!(shape(&groups), vec![(None, vec![movie])]);
    }

    /// `GetLatestItemList`'s music arm reads albums by the scope a music view
    /// stands for, which is that library behind its `DisplayParentId`.
    #[tokio::test]
    async fn a_music_view_takes_the_albums_arm() {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xC300 + n);
        let (view, library, album, track) = (id(1), id(2), id(11), id(12));
        let user = crate::test_support::seed_user_with_defaults(&db, id(3)).await;
        seed_library(&db, library, "Music", Some("music")).await;
        seed_item_with_data(
            &db,
            view,
            BaseItemKind::UserView,
            "Music",
            &format!(
                r#"{{"ViewType":"music","DisplayParentId":"{}"}}"#,
                library.simple()
            ),
        )
        .await;
        seed(
            &db,
            &[
                Row::new(album, BaseItemKind::MusicAlbum, "Album", day(3), library),
                Row::new(track, BaseItemKind::Audio, "Track", day(4), library)
                    .under(album)
                    .on_album("Album"),
            ],
        )
        .await;

        let groups = latest(
            &manager(&db),
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(view),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert_eq!(shape(&groups), vec![(None, vec![album])]);
    }

    /// A `DisplayParentId` naming no row stands for no library
    /// (`GetTopParentIdsForQuery` returns nothing), not for every library.
    #[tokio::test]
    async fn a_view_of_a_missing_library_lists_nothing() {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xC400 + n);
        let (view, library) = (id(1), id(2));
        let user = crate::test_support::seed_user_with_defaults(&db, id(3)).await;
        seed_library(&db, library, "Movies", Some("movies")).await;
        seed_item_with_data(
            &db,
            view,
            BaseItemKind::UserView,
            "Movies",
            &format!(
                r#"{{"ViewType":"movies","DisplayParentId":"{}"}}"#,
                id(4).simple()
            ),
        )
        .await;
        seed(
            &db,
            &[Row::new(id(11), BaseItemKind::Movie, "M", day(3), library)],
        )
        .await;

        let groups = latest(
            &manager(&db),
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(view),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert!(groups.is_empty(), "{:?}", shape(&groups));
    }

    /// The Live TV view is the one view that stands for itself
    /// (`GetTopParentIdsForQuery`'s first arm): its channels carry it as
    /// their top parent.
    #[tokio::test]
    async fn a_live_tv_view_lists_what_it_is_the_top_parent_of() {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xC000 + n);
        let view = id(1);
        let user = crate::test_support::seed_user_with_defaults(&db, id(2)).await;
        seed_item_with_data(
            &db,
            view,
            BaseItemKind::UserView,
            "Live TV",
            r#"{"ViewType":"livetv"}"#,
        )
        .await;
        let channel = id(11);
        seed(
            &db,
            &[Row::new(
                channel,
                BaseItemKind::TvChannel,
                "BBC",
                day(3),
                view,
            )],
        )
        .await;

        let groups = latest(
            &manager(&db),
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(view),
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert_eq!(shape(&groups), vec![(None, vec![channel])]);
    }

    /// A folders view stands for no library, so upstream's scope is the view
    /// itself and the row is empty. A browse of the view does list the root
    /// folders; Latest must not, even asked for that kind by name.
    #[rstest]
    #[case(vec![])]
    #[case(vec![BaseItemKind::CollectionFolder])]
    #[tokio::test]
    async fn a_folders_view_lists_nothing(#[case] include_item_types: Vec<BaseItemKind>) {
        let db = test_db().await;
        let id = |n: u128| Uuid::from_u128(0xC100 + n);
        let (view, user_root, library) = (id(1), id(2), id(3));
        let user = crate::test_support::seed_user_with_defaults(&db, id(4)).await;
        seed_named_item(
            &db,
            user_root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
        )
        .await;
        seed_item_with_data(
            &db,
            view,
            BaseItemKind::UserView,
            "Folders",
            r#"{"ViewType":"folders"}"#,
        )
        .await;
        seed(
            &db,
            &[
                Row::new(
                    library,
                    BaseItemKind::CollectionFolder,
                    "Movies",
                    day(1),
                    user_root,
                ),
                Row::new(id(11), BaseItemKind::Movie, "M", day(3), library),
            ],
        )
        .await;
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        let repository = FerrofinItemRepository::new(db.clone(), lookup).with_root_ids(
            crate::aggregate_folder::RootFolderIds {
                user_root,
                aggregate: id(5),
            },
        );

        let groups = latest(
            &FerrofinUserViewManager::new(Arc::new(repository)),
            LatestItemsQuery {
                user: Some(Arc::new(user)),
                parent_id: Some(view),
                include_item_types,
                limit: Some(20),
                ..LatestItemsQuery::default()
            },
        )
        .await;

        assert!(groups.is_empty(), "{:?}", shape(&groups));
    }

    /// Every library is not a fallback for an evaluator that was never wired.
    #[tokio::test]
    async fn the_user_root_children_refuse_to_answer_without_an_evaluator() {
        let db = test_db().await;
        seed_named_item(&db, MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        let user = crate::test_support::seed_user_with_defaults(&db, Uuid::from_u128(0xAA21)).await;
        let error = manager(&db)
            .get_user_root_children(&user)
            .await
            .expect_err("no evaluator");
        assert!(error.to_string().contains("item visibility"), "{error}");
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

    /// `PlaylistsFolder.CollectionType => playlists` (v10.11.8
    /// `Emby.Server.Implementations/Playlists/PlaylistsFolder.cs:29`), under
    /// the stored spelling and the `GetClientTypeName()` one alike — a
    /// `CollectionFolder` reports its configured library type, and every other
    /// kind reports `None`.
    #[test]
    fn a_playlists_parent_reports_the_playlists_collection_type() {
        let library = Uuid::new_v4();
        let playlists = Uuid::new_v4();
        let types: HashMap<Uuid, Option<CollectionType>> =
            [(library, Some(CollectionType::movies))]
                .into_iter()
                .collect();

        for kind in [
            BaseItemKind::PlaylistsFolder,
            BaseItemKind::ManualPlaylistsFolder,
        ] {
            assert_eq!(
                latest_parent_collection_type(Some(kind), playlists, &types, None),
                Some(CollectionType::playlists),
                "{kind:?}"
            );
        }
        assert_eq!(
            latest_parent_collection_type(
                Some(BaseItemKind::CollectionFolder),
                library,
                &types,
                None
            ),
            Some(CollectionType::movies),
            "a library still reports its own configured type"
        );
        // `BasePluginFolder.CollectionType => null` (BasePluginFolder.cs:14):
        // it is an `ICollectionFolder` WITHOUT a collection type, so the two
        // predicates deliberately disagree on it.
        assert_eq!(
            latest_parent_collection_type(
                Some(BaseItemKind::BasePluginFolder),
                playlists,
                &types,
                None
            ),
            None
        );
        assert!(parent(BaseItemKind::BasePluginFolder, None).is_collection_folder());
        assert_eq!(
            latest_parent_collection_type(None, playlists, &types, None),
            None
        );
    }

    /// `UserView.CollectionType => ViewType` (UserView.cs:58), read from `Data`
    /// the way [`view_type_of`](crate::user_view_repository::view_type_of)
    /// reads it: a name in any case or an adopted blob's discriminant. No
    /// readable value leaves it null, as on upstream's nullable `ViewType`.
    #[rstest]
    #[case(Some(r#"{"ViewType":"movies"}"#), Some(CollectionType::movies))]
    #[case(Some(r#"{"ViewType":"tvshows"}"#), Some(CollectionType::tvshows))]
    #[case(Some(r#"{"ViewType":"music"}"#), Some(CollectionType::music))]
    #[case(Some(r#"{"ViewType":"playlists"}"#), Some(CollectionType::playlists))]
    #[case(Some(r#"{"ViewType":"livetv"}"#), Some(CollectionType::livetv))]
    #[case(Some(r#"{"ViewType":"folders"}"#), Some(CollectionType::folders))]
    #[case(Some(r#"{"ViewType":"unknown"}"#), Some(CollectionType::unknown))]
    #[case(Some(r#"{"ViewType":"TvShows"}"#), Some(CollectionType::tvshows))]
    #[case(Some(r#"{"ViewType":2}"#), Some(CollectionType::tvshows))]
    #[case(Some(r#"{"ViewType":"nonsense"}"#), None)]
    #[case(Some(r#"{"ViewType":null}"#), None)]
    #[case(Some(r#"{"UserId":"x"}"#), None)]
    #[case(Some("not json"), None)]
    #[case(None, None)]
    fn a_view_parent_reports_its_view_type(
        #[case] data: Option<&str>,
        #[case] expected: Option<CollectionType>,
    ) {
        assert_eq!(
            latest_parent_collection_type(
                Some(BaseItemKind::UserView),
                Uuid::new_v4(),
                &HashMap::new(),
                data
            ),
            expected
        );
    }

    /// `PlaylistsFolder : BasePluginFolder : ICollectionFolder` (v10.11.8
    /// `MediaBrowser.Controller/Entities/BasePluginFolder.cs:12`), so a
    /// playlists parent must classify as an `ICollectionFolder` — that is what
    /// puts it on the `top_parent_ids` branch of
    /// `LibraryManager.SetTopParentIdsOrAncestors` instead of the ancestor
    /// closure.
    ///
    /// Pinned under BOTH spellings because this classifies a **stored** row:
    /// 10.11.8 persists `…Playlists.PlaylistsFolder`, while
    /// `ManualPlaylistsFolder` is only `GetClientTypeName()` and the spelling
    /// an older Ferrofin wrote. Recognising just one of them silently moved
    /// `GET /Items/Latest?parentId=<playlistsFolder>` onto the wrong branch.
    #[test]
    fn the_playlists_folder_classifies_as_a_collection_folder() {
        for kind in [
            BaseItemKind::PlaylistsFolder,
            BaseItemKind::ManualPlaylistsFolder,
            BaseItemKind::BasePluginFolder,
            BaseItemKind::CollectionFolder,
        ] {
            assert!(
                parent(kind, None).is_collection_folder(),
                "{kind:?} is an ICollectionFolder"
            );
        }
        // A `UserView` is the other branch, and an ordinary folder is neither
        // — so the assertion above is not a predicate that says yes to
        // everything.
        assert!(!parent(BaseItemKind::UserView, None).is_collection_folder());
        assert!(parent(BaseItemKind::UserView, None).is_user_view());
        assert!(!parent(BaseItemKind::Folder, None).is_collection_folder());
        assert!(!parent(BaseItemKind::Series, None).is_collection_folder());
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
        let found = exclude_item_types_for(&[BaseItemKind::Movie], &[]);
        assert!(found.is_empty(), "{found:?}");
        let found = exclude_item_types_for(&[], &[MediaType::Video]);
        assert!(found.is_empty(), "{found:?}");
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
        provision_at_startup(&db, tmp.path()).await;
        let mgr = media_folders_manager(&db, tmp.path().join("root").join("default"));

        let folders = mgr.get_media_folders().await.expect("media folders");

        // The user-root children are the libraries plus the Playlists folder
        // startup provisioned (C# GetUserRootFolder().Children).
        assert_eq!(folders.len(), libraries + 1);
        let playlists = folders
            .iter()
            .find(|f| {
                // Stored as `PlaylistsFolder` — 10.11.8 has no
                // `ManualPlaylistsFolder` class, that name is only the folder's
                // `GetClientTypeName()`, which `client_kind` applies on the way out.
                f.type_ == "Emby.Server.Implementations.Playlists.PlaylistsFolder"
                    && f.name.as_deref() == Some("Playlists")
            })
            .expect("Playlists media folder present");
        // Stored as upstream stores it, `%AppDataPath%/playlists` (gl-1581);
        // this test's repository reads it as stored, the server's expands it.
        assert_eq!(playlists.path.as_deref(), Some("%AppDataPath%/playlists"));
        // The backing directory is created on disk.
        assert!(playlists_path.is_dir());

        // The id is stored the way Jellyfin stores Guid columns: UPPERCASE
        // hyphenated. `BaseItems."Id"` is plain TEXT with no COLLATE NOCASE, so
        // a lowercase id is a different row to SQLite — an existence check by
        // id would never match it, and the folder would also come back to
        // clients with a lowercase `Id` where Jellyfin sends uppercase.
        assert_eq!(
            playlists.id,
            playlists.id.to_uppercase(),
            "the provisioned id must be stored in guid_to_db form, got {}",
            playlists.id
        );

        // A read never WRITES the folder, not merely never duplicates it. An
        // upsert on the same id can never duplicate, so a row count proves
        // nothing; the bug this guards against re-ran the upsert on every
        // single request and still left exactly one row. Renaming the row out
        // from under the manager makes a rewrite observable: if a read
        // provisions it again it stamps `Name` back to "Playlists".
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        let mut renamed_row = playlists.clone();
        renamed_row.name = Some("SENTINEL".to_owned());
        persistence
            .save_items(std::slice::from_ref(&renamed_row))
            .await
            .expect("rename the provisioned row");

        let again = mgr.get_media_folders().await.expect("media folders again");
        assert_eq!(again.len(), libraries + 1);
        let renamed = again
            .iter()
            .find(|f| f.id == playlists.id)
            .expect("the provisioned row is still there");
        assert_eq!(
            renamed.name.as_deref(),
            Some("SENTINEL"),
            "a read re-provisioned the folder, so every request pays a \
             filesystem call and a write through the single writer connection"
        );
    }

    /// What the composition root runs once before serving anything
    /// (`AggregateFolderStore::ensure`, upstream's `CreateRootFolder`): the
    /// aggregate root at `{tmp}/root` and the playlists folder at
    /// `{tmp}/data/playlists`.
    async fn provision_at_startup(db: &Database, tmp: &std::path::Path) {
        crate::AggregateFolderStore::new(
            db.clone(),
            item_type_lookup::IdDerivation::LegacyLowercase,
            tmp.join("root"),
            tmp.join("data"),
        )
        .ensure()
        .await
        .expect("startup provisioning");
    }

    /// gl-343: startup keeps a playlists folder it finds at the folder's
    /// path under another id, and the media folders list that one. Nothing
    /// on the request path provisions a second beside it, so the folder is
    /// listed once and every playlist stays in it.
    #[tokio::test]
    async fn media_folders_keep_a_playlists_folder_found_by_path() {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let db = test_db().await;
            let tmp = tempfile::tempdir().expect("tempdir");
            let playlists_path = tmp.path().join("data").join("playlists");
            let foreign = Uuid::from_u128(0xADA);
            seed_named_item(&db, foreign, BaseItemKind::PlaylistsFolder, "Playlists").await;
            set_item_path(&db, foreign, &playlists_path.to_string_lossy()).await;
            provision_at_startup(&db, tmp.path()).await;
            let mgr = media_folders_manager(&db, tmp.path().join("root").join("default"));

            for _ in 0..2 {
                let folders = mgr.get_media_folders().await.expect("media folders");
                let playlists: Vec<_> = folders
                    .iter()
                    .filter(|f| {
                        matches!(
                            item_type_lookup::kind_from_type_name(&f.type_),
                            Some(
                                BaseItemKind::PlaylistsFolder | BaseItemKind::ManualPlaylistsFolder
                            )
                        )
                    })
                    .map(|f| f.id.clone())
                    .collect();
                assert_eq!(
                    playlists,
                    vec![guid_to_db(foreign)],
                    "one playlists folder, the one already at the path"
                );
            }
            assert_eq!(
                crate::test_support::items_at_path(&db, &playlists_path.to_string_lossy()).await,
                1,
                "no second playlists folder was written at that path"
            );
        })
        .await
        .expect("the test finishes");
    }

    /// The per-user named-view id, pinned against a real Jellyfin 12.0
    /// database: one user, the `PlaylistsFolder`, and the Playlists view
    /// Jellyfin 12.0.0 created on its first boot (the 10.11 row beside it,
    /// `AA9A5206-…`, is the name-keyed one 12.0 ignores).
    #[tokio::test]
    async fn user_named_view_id_matches_a_real_12_0_row() {
        let db = test_db().await;
        let mgr = FerrofinUserViewManager::new(crate::test_support::item_repository_over(db))
            .with_id_derivation(item_type_lookup::IdDerivation::Jellyfin {
                program_data_path: Some("/config".to_owned()),
            });
        let user = Uuid::parse_str("9ECCC7EC-621F-47CF-ADCB-3CD4A0C29227").expect("user");
        let folder = Uuid::parse_str("1071671E-7BFF-A053-2E93-0DEBEE501D2E").expect("folder");
        assert_eq!(
            mgr.user_named_view_id(user, folder, "playlists"),
            Some(Uuid::parse_str("45D83C7B-5EDC-AF5C-B7E4-6C1612C07B6A").expect("view"))
        );
    }

    struct DashboardConfiguration(
        std::sync::Mutex<ferrofin_model::configuration::ServerConfiguration>,
        tempfile::TempDir,
    );

    impl Default for DashboardConfiguration {
        fn default() -> Self {
            Self(
                std::sync::Mutex::new(ferrofin_model::configuration::ServerConfiguration::default()),
                tempfile::tempdir().expect("dashboard application paths"),
            )
        }
    }

    #[async_trait]
    impl ServerConfigurationManager for DashboardConfiguration {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            crate::app_paths::test_paths(self.1.path())
        }
        async fn configuration(
            &self,
        ) -> Result<Arc<ferrofin_model::configuration::ServerConfiguration>, ServiceError> {
            Ok(Arc::new(self.0.lock().unwrap().clone()))
        }
        async fn update_configuration(
            &self,
            value: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            *self.0.lock().unwrap() = value.clone();
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// gl-385: the Live TV view is named the server-localized `HeaderLiveTV`
    /// whatever culture the request runs under, and renamed when the server's
    /// language changes (v12.2 `GetInternalLiveTvFolder` → `GetNamedView`,
    /// LiveTvManager.cs:1271-1275, LibraryManager.cs:3036-3042).
    #[tokio::test]
    async fn the_live_tv_view_takes_the_server_culture_and_follows_it() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(DashboardConfiguration::default());
        let localization = Arc::new(
            crate::LocalizationManager::new("US").with_ui_culture_source({
                let config = Arc::clone(&config);
                move || config.0.lock().unwrap().ui_culture.clone()
            }),
        );
        let manager = manager(&db)
            .with_item_store(Arc::new(FerrofinItemPersistenceService::new(db.clone())))
            .with_metadata_path(tmp.path().join("metadata"))
            .with_database(db.clone())
            .with_dashboard_configuration(config.clone(), localization);
        let provision = || {
            ferrofin_util::current_culture::scope_with_ui(
                "fr".into(),
                Some("fr".into()),
                manager.get_internal_live_tv_folder_id(),
            )
        };
        let names = |row: BaseItemEntity| (row.name, row.forced_sort_name);

        let id = provision().await.expect("provision").expect("an id");
        let row = crate::test_support::fetch_item(&db, id).await;
        let english = Some("Live TV".to_owned());
        assert_eq!(names(row), (english.clone(), english));

        config.0.lock().unwrap().ui_culture = "fr".to_owned();
        assert_eq!(provision().await.expect("provision"), Some(id));
        let row = crate::test_support::fetch_item(&db, id).await;
        let french = Some("TV en direct".to_owned());
        assert_eq!(names(row), (french.clone(), french));
    }

    /// gl-1322: a views read finds the Live TV row among the view rows it
    /// lists, so the read that renames it after a language change lists the
    /// new name, as upstream's `GetNamedView` returns the item it renamed.
    #[tokio::test]
    async fn the_views_read_that_renames_the_live_tv_view_lists_the_new_name() {
        let db = test_db().await;
        let user_id = Uuid::from_u128(0x5111);
        seed_user(&db, user_id).await;
        let mut tx = db.writer().begin().await.expect("begin");
        crate::user_entity_ext::seed_defaults(&mut tx, &guid_to_db(user_id))
            .await
            .expect("permissions");
        tx.commit().await.expect("commit");
        db.upsert_live_tv_tuner_host("t1", "http://tuner", "m3u", "{}")
            .await
            .expect("tuner");
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(DashboardConfiguration::default());
        config.0.lock().unwrap().enable_folder_view = false;
        let localization = Arc::new(
            crate::LocalizationManager::new("US").with_ui_culture_source({
                let config = Arc::clone(&config);
                move || config.0.lock().unwrap().ui_culture.clone()
            }),
        );
        let manager = manager(&db)
            .with_item_store(Arc::new(FerrofinItemPersistenceService::new(db.clone())))
            .with_metadata_path(tmp.path().join("metadata"))
            .with_database(db.clone())
            .with_dashboard_configuration(config.clone(), localization);
        let names = |views: Vec<BaseItemEntity>| {
            views
                .into_iter()
                .filter_map(|v| v.name)
                .collect::<Vec<String>>()
        };

        assert_eq!(
            names(manager.get_user_views(user_id).await.expect("views")),
            ["Live TV"],
            "the read that makes the row lists it"
        );
        config.0.lock().unwrap().ui_culture = "fr".to_owned();
        assert_eq!(
            names(manager.get_user_views(user_id).await.expect("views")),
            ["TV en direct"],
            "the read that renames the row lists the new name"
        );
    }

    // One live sequence verifies identity across toggle, localization and hidden preferences.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn folder_view_toggle_localization_and_hidden_preferences_are_live_and_keep_the_id() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(DashboardConfiguration::default());
        config.0.lock().unwrap().enable_folder_view = false;
        let localization = Arc::new(
            crate::LocalizationManager::new("US").with_ui_culture_source({
                let config = Arc::clone(&config);
                move || config.0.lock().unwrap().ui_culture.clone()
            }),
        );
        let manager = manager(&db)
            .with_item_store(Arc::new(FerrofinItemPersistenceService::new(db.clone())))
            .with_metadata_path(tmp.path().join("metadata"))
            .with_database(db.clone())
            .with_dashboard_configuration(config.clone(), localization);
        let stale = Uuid::new_v4();
        seed_item_with_data(
            &db,
            stale,
            BaseItemKind::UserView,
            "Stale folders",
            r#"{"ViewType":"folders"}"#,
        )
        .await;
        assert!(
            !manager
                .get_user_views_with_query(user, &hidden_too())
                .await
                .unwrap()
                .iter()
                .any(is_folders_view_row),
            "disabled hides persisted/adopted folder views too"
        );
        config.0.lock().unwrap().enable_folder_view = true;
        let views = manager.get_user_views(user).await.unwrap();
        let folders: Vec<_> = views
            .iter()
            .filter(|row| is_folders_view_row(row))
            .collect();
        assert_eq!(
            folders.len(),
            1,
            "only the canonical named folders view is appended"
        );
        let id = Uuid::parse_str(&folders[0].id).unwrap();
        assert_ne!(id, stale);
        assert_eq!(folders[0].name.as_deref(), Some("Folders"));
        assert!(tmp.path().join("metadata/views/folders").is_dir());
        let stored = manager.items.retrieve_item(id).await.unwrap().unwrap();
        manager.get_user_views(user).await.unwrap();
        assert_eq!(
            manager
                .items
                .retrieve_item(id)
                .await
                .unwrap()
                .unwrap()
                .date_last_saved,
            stored.date_last_saved,
            "warm reads do not rewrite the view"
        );
        config.0.lock().unwrap().ui_culture = "fr".to_owned();
        let localized = manager
            .get_user_views(user)
            .await
            .unwrap()
            .into_iter()
            .find(is_folders_view_row)
            .unwrap();
        assert_eq!(Uuid::parse_str(&localized.id).unwrap(), id);
        assert_eq!(localized.name.as_deref(), Some("Dossiers"));
        assert_eq!(localized.sort_name.as_deref(), Some("dossiers"));
        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user),
            ferrofin_db::enums::PreferenceKind::MyMediaExcludes,
            &[guid_to_db(id)],
        )
        .await
        .unwrap();
        assert!(
            !manager
                .get_user_views(user)
                .await
                .unwrap()
                .iter()
                .any(is_folders_view_row)
        );
        assert!(
            manager
                .get_user_views_with_query(user, &hidden_too())
                .await
                .unwrap()
                .iter()
                .any(is_folders_view_row)
        );
        config.0.lock().unwrap().enable_folder_view = false;
        assert!(
            !manager
                .get_user_views_with_query(user, &hidden_too())
                .await
                .unwrap()
                .iter()
                .any(is_folders_view_row)
        );
        assert!(
            manager.items.retrieve_item(id).await.unwrap().is_some(),
            "disabled does not discard the stored identity/preferences"
        );
        config.0.lock().unwrap().enable_folder_view = true;
        assert!(
            !manager
                .get_user_views(user)
                .await
                .unwrap()
                .iter()
                .any(is_folders_view_row),
            "hidden preference survives disable/re-enable"
        );
        assert_eq!(
            manager
                .get_user_views_with_query(user, &hidden_too())
                .await
                .unwrap()
                .into_iter()
                .find(is_folders_view_row)
                .unwrap()
                .id,
            guid_to_db(id)
        );
    }
    #[test]
    fn folders_view_accepts_adopted_numeric_type_but_not_explicit_null_or_other_type() {
        let user_view = item_type_lookup::stored_type_name(BaseItemKind::UserView).unwrap();
        let path = Some("%MetadataPath%\\views\\folders");
        assert!(is_folders_view_fields(
            user_view,
            path,
            Some(r#"{"ViewType":12}"#)
        ));
        assert!(is_folders_view_fields(
            user_view,
            path,
            Some(r#"{"ViewType":"FOLDERS"}"#)
        ));
        assert!(is_folders_view_fields(user_view, path, None));
        assert!(!is_folders_view_fields(
            user_view,
            path,
            Some(r#"{"ViewType":null}"#)
        ));
        assert!(!is_folders_view_fields(
            user_view,
            path,
            Some(r#"{"ViewType":1}"#)
        ));
        assert!(!is_folders_view_fields(
            item_type_lookup::stored_type_name(BaseItemKind::Folder).unwrap(),
            path,
            Some(r#"{"ViewType":12}"#)
        ));
    }

    #[tokio::test]
    async fn adopted_numeric_folder_view_keeps_metadata_relative_id_and_warm_timestamp() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let tmp = tempfile::tempdir().unwrap();
        let metadata = tmp.path().join("metadata");
        let mode = item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
        };
        let id = crate::user_view_repository::named_view_id(&mode, &metadata, "folders").unwrap();
        let moved_mode = item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: Some("/relocated/programdata".to_owned()),
        };
        assert_eq!(
            crate::user_view_repository::named_view_id(
                &moved_mode,
                std::path::Path::new("/relocated/programdata/metadata"),
                "folders"
            ),
            Some(id)
        );
        seed_item_with_data(
            &db,
            id,
            BaseItemKind::UserView,
            "Folders",
            r#"{"ViewType":12}"#,
        )
        .await;
        crate::user_view_repository::seed_adopted_folder_view(&db, id)
            .await
            .unwrap();
        let config = Arc::new(DashboardConfiguration::default());
        config.0.lock().unwrap().enable_folder_view = true;
        let localization = Arc::new(
            crate::LocalizationManager::new("US").with_ui_culture_source({
                let config = Arc::clone(&config);
                move || config.0.lock().unwrap().ui_culture.clone()
            }),
        );
        let manager = manager(&db)
            .with_item_store(Arc::new(FerrofinItemPersistenceService::new(db.clone())))
            .with_metadata_path(metadata.clone())
            .with_id_derivation(mode)
            .with_dashboard_configuration(config.clone(), localization);
        let before = manager.items.retrieve_item(id).await.unwrap().unwrap();
        for _ in 0..2 {
            let views = manager.get_user_views(user).await.unwrap();
            let folder = views
                .iter()
                .filter(|view| is_folders_view_row(view))
                .collect::<Vec<_>>();
            assert_eq!(folder.len(), 1);
            assert_eq!(folder[0].id, guid_to_db(id));
            assert_eq!(
                folder[0].path.as_deref(),
                Some(metadata.join("views/folders").to_string_lossy().as_ref())
            );
        }
        let after = manager.items.retrieve_item(id).await.unwrap().unwrap();
        assert_eq!(after.date_last_saved, before.date_last_saved);
        assert_eq!(
            after.path.as_deref(),
            Some("%MetadataPath%\\views\\folders"),
            "warm token expansion is read-only"
        );
        config.0.lock().unwrap().ui_culture = "fr".to_owned();
        let renamed = manager
            .get_user_views(user)
            .await
            .unwrap()
            .into_iter()
            .find(is_folders_view_row)
            .unwrap();
        assert_eq!(renamed.id, guid_to_db(id));
        assert_eq!(renamed.name.as_deref(), Some("Dossiers"));
        assert_ne!(renamed.date_last_saved, before.date_last_saved);
        config.0.lock().unwrap().enable_folder_view = false;
        assert!(
            !manager
                .get_user_views(user)
                .await
                .unwrap()
                .iter()
                .any(is_folders_view_row)
        );
        assert!(manager.items.retrieve_item(id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn appended_folder_view_uses_pinned_ordinal_ignore_case_home_order() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(DashboardConfiguration::default());
        config.0.lock().unwrap().enable_folder_view = true;
        let localization = Arc::new(crate::LocalizationManager::new("US"));
        let zulu = Uuid::new_v4();
        seed_item_with_data(&db, zulu, BaseItemKind::CollectionFolder, "Zulu", "{}").await;
        crate::user_view_repository::seed_view_sort_name(&db, zulu, "ZULU")
            .await
            .unwrap();
        let manager = manager(&db)
            .with_item_store(Arc::new(FerrofinItemPersistenceService::new(db.clone())))
            .with_metadata_path(tmp.path().join("metadata"))
            .with_dashboard_configuration(config, localization);
        let views = manager.get_user_views(user).await.unwrap();
        let folders = views.iter().position(is_folders_view_row).unwrap();
        let zulu = views
            .iter()
            .position(|view| Uuid::parse_str(&view.id).ok() == Some(zulu))
            .unwrap();
        assert!(
            folders < zulu,
            "source SortNameComparer folds case before comparing"
        );
    }

    /// A library row of `collection_type` (its `Data`'s `CollectionType`,
    /// `None` for one written without it).
    async fn seed_library(db: &Database, id: Uuid, name: &str, collection_type: Option<&str>) {
        let row = BaseItemEntity {
            id: guid_to_db(id),
            type_: stored_type_name(BaseItemKind::CollectionFolder)
                .unwrap_or_default()
                .to_owned(),
            name: Some(name.to_owned()),
            sort_name: Some(create_sort_name(name)),
            is_folder: true,
            data: collection_type.map(|t| format!(r#"{{"CollectionType":"{t}"}}"#)),
            ..BaseItemEntity::default()
        };
        FerrofinItemPersistenceService::new(db.clone())
            .save_items(std::slice::from_ref(&row))
            .await
            .expect("seed library");
    }

    /// A manager that can make grouped views: preferences, an item store and
    /// a metadata path.
    fn grouping_manager(db: &Database, metadata: &std::path::Path) -> FerrofinUserViewManager {
        manager(db)
            .with_database(db.clone())
            .with_item_store(Arc::new(FerrofinItemPersistenceService::new(db.clone())))
            .with_metadata_path(metadata.to_path_buf())
    }

    async fn group(db: &Database, user: Uuid, libraries: &[Uuid]) {
        let ids: Vec<String> = libraries.iter().map(|id| guid_to_db(*id)).collect();
        crate::user_entity_ext::set_preference(
            db,
            &guid_to_db(user),
            ferrofin_db::enums::PreferenceKind::GroupedFolders,
            &ids,
        )
        .await
        .expect("save GroupedFolders");
    }

    async fn view_names(mgr: &FerrofinUserViewManager, user: Uuid) -> Vec<String> {
        mgr.get_user_views(user)
            .await
            .expect("views")
            .into_iter()
            .filter_map(|view| view.name)
            .collect()
    }

    /// `UserViewQuery.PresetViews` (`UserViewManager.cs:87-94`, v12.4): a
    /// library of a named type is listed in place as its shadow view
    /// (`GetShadowView`), made once with the library's name, path,
    /// `ViewType` and `DisplayParentId`; an untyped library never is, and one
    /// with no path stays the library. `MyMediaExcludes` hides a view by its
    /// own id, so hiding a library leaves its shadow view listed, and the
    /// plain listing keeps the libraries.
    #[tokio::test]
    async fn preset_views_list_a_library_of_a_named_type_as_its_shadow_view() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let user = Uuid::from_u128(0x7801);
        seed_user(&db, user).await;
        let (films, songs, mixed, bare) = (
            Uuid::from_u128(0x7811),
            Uuid::from_u128(0x7812),
            Uuid::from_u128(0x7813),
            Uuid::from_u128(0x7814),
        );
        seed_library(&db, films, "Films", Some("movies")).await;
        seed_library(&db, songs, "Songs", Some("music")).await;
        seed_library(&db, mixed, "Mixed", Some("mixed")).await;
        seed_library(&db, bare, "Bare", Some("movies")).await;
        for (id, name) in [(films, "Films"), (songs, "Songs"), (mixed, "Mixed")] {
            let path = tmp.path().join("root").join(name);
            set_item_path(&db, id, path.to_str().expect("utf-8")).await;
        }
        let mgr = grouping_manager(&db, &tmp.path().join("metadata"));
        let shadow = |name: &str, id: Uuid, view_type: &str| {
            shadow_view_id(&mgr.id_derivation, name, id, view_type).expect("a shadow id")
        };
        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user),
            ferrofin_db::enums::PreferenceKind::MyMediaExcludes,
            &[
                guid_to_db(films),
                guid_to_db(shadow("Songs", songs, "music")),
            ],
        )
        .await
        .expect("save MyMediaExcludes");
        let listed = |views: Vec<BaseItemEntity>| {
            views
                .iter()
                .map(|v| (v.name.clone().unwrap_or_default(), v.type_.clone(), uuid(v)))
                .collect::<Vec<_>>()
        };
        let folder = stored_type_name(BaseItemKind::CollectionFolder)
            .unwrap_or_default()
            .to_owned();
        let view = stored_type_name(BaseItemKind::UserView)
            .unwrap_or_default()
            .to_owned();
        let preset = UserViewQuery {
            preset_views: vec![
                CollectionType::movies,
                CollectionType::music,
                CollectionType::tvshows,
            ],
            ..UserViewQuery::default()
        };

        let views = mgr
            .get_user_views_with_query(user, &preset)
            .await
            .expect("views");
        let made = views
            .iter()
            .find(|v| v.name.as_deref() == Some("Films"))
            .expect("the Films view")
            .clone();
        assert_eq!(
            listed(views),
            [
                ("Bare".to_owned(), folder.clone(), bare),
                (
                    "Films".to_owned(),
                    view.clone(),
                    shadow("Films", films, "movies")
                ),
                ("Mixed".to_owned(), folder.clone(), mixed),
            ],
            "the Songs shadow view is hidden by its own id, the Films one is not"
        );
        let data = crate::item_data::parse_data(made.data.as_deref());
        assert_eq!(
            crate::item_data::read_data_string(&data, "ViewType").as_deref(),
            Some("movies")
        );
        assert_eq!(
            crate::item_data::read_data_string(&data, "DisplayParentId")
                .and_then(|id| Uuid::parse_str(&id).ok()),
            Some(films)
        );
        assert_eq!(
            made.path,
            crate::test_support::fetch_item(&db, films).await.path
        );
        assert_eq!(made.parent_id, None);
        assert!(made.date_created.is_some() && made.date_modified.is_some());

        // The plain listing keeps the libraries (and hides Films by its id).
        assert_eq!(
            listed(mgr.get_user_views(user).await.expect("views")),
            [
                ("Bare".to_owned(), folder.clone(), bare),
                ("Mixed".to_owned(), folder.clone(), mixed),
                ("Songs".to_owned(), folder, songs),
            ]
        );
    }

    /// `if (query.IncludeExternalContent)` guards `GetInternalLiveTvFolder`,
    /// so a request without external content neither lists nor creates the
    /// Live TV view, even for a user with Live TV.
    #[tokio::test]
    async fn without_external_content_the_live_tv_view_is_not_made() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let user = Uuid::from_u128(0x7821);
        seed_user(&db, user).await;
        let mut tx = db.writer().begin().await.expect("begin");
        crate::user_entity_ext::seed_defaults(&mut tx, &guid_to_db(user))
            .await
            .expect("permissions");
        tx.commit().await.expect("commit");
        db.upsert_live_tv_tuner_host("t1", "http://tuner", "m3u", "{}")
            .await
            .expect("tuner");
        let metadata = tmp.path().join("metadata");
        let mgr = grouping_manager(&db, &metadata);
        let live_tv = mgr
            .live_tv_view_id(&metadata.join("views").join("livetv"))
            .expect("the Live TV view id");
        let without = UserViewQuery {
            include_external_content: false,
            ..UserViewQuery::default()
        };

        let views = mgr
            .get_user_views_with_query(user, &without)
            .await
            .expect("views");
        assert!(views.iter().all(|v| uuid(v) != live_tv), "{views:?}");
        assert!(
            mgr.items
                .retrieve_item(live_tv)
                .await
                .expect("read")
                .is_none(),
            "not created"
        );

        let views = mgr.get_user_views(user).await.expect("views");
        assert!(views.iter().any(|v| uuid(v) == live_tv), "{views:?}");
    }

    /// 12.2 `GetUserViews`: the movies, shows and untyped libraries a user
    /// grouped become one Movies and one TV Shows view. An untyped library
    /// joins both; a music library, grouped or not, stays itself, as does
    /// every library left ungrouped. The list stays in name order.
    #[tokio::test]
    async fn grouped_libraries_become_one_view_per_type() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let user = Uuid::from_u128(0x7701);
        seed_user(&db, user).await;
        let (action, comedy, series, mixed, songs) = (
            Uuid::from_u128(0x7711),
            Uuid::from_u128(0x7712),
            Uuid::from_u128(0x7713),
            Uuid::from_u128(0x7714),
            Uuid::from_u128(0x7715),
        );
        seed_library(&db, action, "Action", Some("movies")).await;
        seed_library(&db, comedy, "Comedy", Some("movies")).await;
        seed_library(&db, series, "Series", Some("tvshows")).await;
        seed_library(&db, mixed, "Mixed", Some("mixed")).await;
        seed_library(&db, songs, "Songs", Some("music")).await;
        group(&db, user, &[action, comedy, mixed, songs]).await;
        let mgr = grouping_manager(&db, tmp.path());

        assert_eq!(
            view_names(&mgr, user).await,
            ["Movies", "Series", "Songs", "TvShows"]
        );
        let views = mgr.get_user_views(user).await.expect("views");
        let movies = views
            .iter()
            .find(|view| view.name.as_deref() == Some("Movies"))
            .expect("the Movies view");
        assert_eq!(
            Some(uuid(movies)),
            grouped_view_id(
                &item_type_lookup::IdDerivation::LegacyLowercase,
                user,
                "movies"
            )
        );
        let data = crate::item_data::parse_data(movies.data.as_deref());
        assert_eq!(data["ViewType"], "movies");
        assert_eq!(data["UserId"], user.simple().to_string());
        assert!(data.get("DisplayParentId").is_none());
        assert!(
            tmp.path()
                .join("views")
                .join(uuid(movies).simple().to_string())
                .is_dir(),
            "GetNamedView creates the view's folder"
        );
        // A second read finds the rows, and makes no others.
        assert_eq!(
            view_names(&mgr, user).await,
            ["Movies", "Series", "Songs", "TvShows"]
        );
    }

    /// `GetUserView(parents, …)`: a single grouped library of exactly the
    /// view's type is listed as itself, not as a grouped view.
    #[tokio::test]
    async fn a_single_grouped_library_of_the_type_stays_itself() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let user = Uuid::from_u128(0x7721);
        seed_user(&db, user).await;
        let (action, series) = (Uuid::from_u128(0x7722), Uuid::from_u128(0x7723));
        seed_library(&db, action, "Action", Some("movies")).await;
        seed_library(&db, series, "Series", Some("tvshows")).await;
        group(&db, user, &[action]).await;
        let mgr = grouping_manager(&db, tmp.path());
        assert_eq!(view_names(&mgr, user).await, ["Action", "Series"]);
    }

    /// A grouped view is the user's alone, and only while they group: another
    /// user lists the libraries, and ungrouping brings them back, the stored
    /// view row left out.
    #[tokio::test]
    async fn a_grouped_view_follows_its_users_grouping() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let (user, other) = (Uuid::from_u128(0x7731), Uuid::from_u128(0x7732));
        seed_named_user(&db, user, "grouping").await;
        seed_named_user(&db, other, "other").await;
        let (action, comedy) = (Uuid::from_u128(0x7733), Uuid::from_u128(0x7734));
        seed_library(&db, action, "Action", Some("movies")).await;
        seed_library(&db, comedy, "Comedy", Some("movies")).await;
        group(&db, user, &[action, comedy]).await;
        let mgr = grouping_manager(&db, tmp.path());
        assert_eq!(view_names(&mgr, user).await, ["Movies"]);
        assert_eq!(view_names(&mgr, other).await, ["Action", "Comedy"]);
        group(&db, user, &[]).await;
        assert_eq!(view_names(&mgr, user).await, ["Action", "Comedy"]);
    }

    /// `GetNamedViewId(user, Guid.Empty, viewType)`: no parent part.
    #[test]
    fn the_grouped_view_id_has_no_parent_part() {
        let user = Uuid::from_u128(0x7741);
        let mode = item_type_lookup::IdDerivation::LegacyLowercase;
        assert_eq!(
            grouped_view_id(&mode, user, "tvshows"),
            item_type_lookup::derive_item_id_with(
                &mode,
                BaseItemKind::UserView,
                &format!("38_namedview_{}tvshows", user.simple()),
            )
        );
        assert_ne!(
            grouped_view_id(&mode, user, "tvshows"),
            named_view_id(&mode, user, Uuid::nil(), "tvshows"),
            "a nil parent would add 32 zeros"
        );
    }

    /// A lone untyped grouped library joins both views (`GetUserView` takes
    /// the library itself only when its type is the view's), and several
    /// grouped shows libraries make one TV Shows view.
    #[tokio::test]
    async fn an_untyped_library_joins_both_views_and_shows_group_too() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let user = Uuid::from_u128(0x7751);
        seed_user(&db, user).await;
        let (mixed, anime, drama) = (
            Uuid::from_u128(0x7752),
            Uuid::from_u128(0x7753),
            Uuid::from_u128(0x7754),
        );
        seed_library(&db, mixed, "Mixed", None).await;
        seed_library(&db, anime, "Anime", Some("tvshows")).await;
        seed_library(&db, drama, "Drama", Some("tvshows")).await;
        let mgr = grouping_manager(&db, tmp.path());
        group(&db, user, &[mixed]).await;
        assert_eq!(
            view_names(&mgr, user).await,
            ["Anime", "Drama", "Movies", "TvShows"]
        );
        group(&db, user, &[anime, drama]).await;
        assert_eq!(view_names(&mgr, user).await, ["Mixed", "TvShows"]);
    }

    /// `GetNamedView` renames a stored view whose name no longer reads as the
    /// server-localized one, and the view can be hidden and ordered like any
    /// other (`MyMediaExcludes`, `OrderedViews` by its own id).
    #[tokio::test]
    async fn a_grouped_view_is_renamed_hidden_and_ordered_like_any_view() {
        use ferrofin_db::enums::PreferenceKind;
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let user = Uuid::from_u128(0x7761);
        seed_user(&db, user).await;
        let (action, comedy, series) = (
            Uuid::from_u128(0x7762),
            Uuid::from_u128(0x7763),
            Uuid::from_u128(0x7764),
        );
        seed_library(&db, action, "Action", Some("movies")).await;
        seed_library(&db, comedy, "Comedy", Some("movies")).await;
        seed_library(&db, series, "Series", Some("tvshows")).await;
        group(&db, user, &[action, comedy]).await;
        let mgr = grouping_manager(&db, tmp.path());
        assert_eq!(view_names(&mgr, user).await, ["Movies", "Series"]);

        let id = grouped_view_id(
            &item_type_lookup::IdDerivation::LegacyLowercase,
            user,
            "movies",
        )
        .expect("id");
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        let mut row = mgr
            .items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row");
        row.name = Some("Old Name".to_owned());
        persistence
            .save_items(std::slice::from_ref(&row))
            .await
            .expect("rename");
        assert_eq!(view_names(&mgr, user).await, ["Movies", "Series"]);
        let row = mgr
            .items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.name.as_deref(), Some("Movies"));
        assert_eq!(row.sort_name.as_deref(), Some("movies"));

        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user),
            PreferenceKind::OrderedViews,
            &[guid_to_db(series), guid_to_db(id)],
        )
        .await
        .expect("order");
        assert_eq!(view_names(&mgr, user).await, ["Series", "Movies"]);
        crate::user_entity_ext::set_preference(
            &db,
            &guid_to_db(user),
            PreferenceKind::MyMediaExcludes,
            &[guid_to_db(id)],
        )
        .await
        .expect("hide");
        assert_eq!(view_names(&mgr, user).await, ["Series"]);
    }
}
