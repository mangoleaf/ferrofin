//! [`FerrofinCollectionManager`] + [`FerrofinPlaylistManager`] — **minimal**
//! [`CollectionManager`]/[`PlaylistManager`] impls.
//!
//! Ports of `Emby.Server.Implementations.Collections.CollectionManager` and
//! `Emby.Server.Implementations.Playlists.PlaylistManager`. Both are thin unit-8
//! managers: a box set and a playlist are both `BaseItem` rows, so creating one
//! inserts a minimal `BaseItems` row (of the `BoxSet`/`Playlist` stored type)
//! through `ferrofin-db`, and membership is recorded as manual **linked children**
//! via the injected
//! [`LinkedChildrenService`](ferrofin_traits::persistence::LinkedChildrenService).
//! Reads go through the injected
//! [`LibraryManager`](ferrofin_traits::library::LibraryManager) so the same
//! persistence seam backs everything.
//!
//! Playlist **ownership and access** are real: the `FerrofinPlaylists` table records the
//! owner + open-access flag (C# `Playlist.OwnerUserId`/`OpenAccess`), the
//! `FerrofinPlaylistShares` table records per-user share permissions (`Playlist.Shares`),
//! and every read resolves the caller's [`PlaylistAccess`] — owner, `CanEdit`
//! share, read-only share, or open-access — with invisible playlists reported as
//! `NotFound` (the C# `GetPlaylistForUser` null). A playlist with no `Playlists`
//! row (created before owner tracking), or with no owner in it, grants `Owner`
//! to every caller for back-compat. A create or lookup with no user is
//! refused, as upstream's `GetUserById` does.
//!
//! A playlist the library scan resolved from a `.m3u`/`.pls` file is owned by
//! nobody and open to everyone (`PlaylistResolver`), so every caller reads it
//! and none may change it — upstream's `isPermitted` is the owner or an
//! editing share. That is why `SavePlaylistFile`, which upstream runs after
//! each change to a playlist FILE, has no caller here: ordering lives in the
//! linked-children rows.
//!
//! A playlist entry id is its item's id as a lower-case `Guid('N')`, as v12.2's
//! `GetPlaylistItems` and `RemoveItemFromPlaylistAsync`/`MoveItemAsync`
//! address entries (`ItemId?.ToString("N")`).

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities_media::PlaylistUserPermissions;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::playlists::{
    PlaylistCreationRequest, PlaylistCreationResult, PlaylistUpdateRequest,
    PlaylistUserUpdateRequest,
};
use uuid::Uuid;

use ferrofin_traits::collections::{
    CollectionCreationOptions, CollectionManager, PlaylistAccess, PlaylistAccessLevel,
    PlaylistManager,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, SelfWrite, SelfWriteGuard, UserManager};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, LinkedChildrenService};
use ferrofin_traits::providers::{
    MetadataRefreshMode, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use ferrofin_traits::system::ServerApplicationPaths;

use crate::db_error::db_err;
use crate::item_data;
use crate::item_type_lookup::{kind_from_type_name, stored_type_name};

/// The manual [`LinkedChildType`](ferrofin_db::enums::LinkedChildType) discriminant
/// (`0`) used to record a box-set/playlist member.
const LINKED_CHILD_MANUAL: i32 = 0;

/// How a membership change re-rates its box set or playlist upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rerate {
    /// `CollectionManager.AddToCollectionAsync` (`:271`) calls
    /// `UpdateRatingToItems` directly: an `IsLocked` box set — jellyfin-web
    /// creates them locked unless "search for internet metadata" is ticked —
    /// still takes its members' rating, and a failure fails the request.
    Direct,
    /// The `ForceSave` refresh upstream queues after every other membership
    /// change (`MetadataService.UpdateOfficialRating`, `:623-635`): a locked
    /// item keeps its rating, and a failure is logged, never the request's.
    Refresh,
}

/// `_providerManager.QueueRefresh(id, options, RefreshPriority.High)`, which
/// upstream queues after every box-set and playlist membership change, so the
/// container's metadata, images (a box set's TMDB overview and artwork, a
/// playlist's collage) and rating follow its members. Nothing is queued
/// without a provider manager (unit tests).
async fn queue_refresh(
    providers: Option<&dyn ProviderManager>,
    container: Uuid,
    options: MetadataRefreshOptions,
) -> Result<(), ServiceError> {
    match providers {
        Some(providers) => {
            providers
                .queue_refresh(container, &options, RefreshPriority::High)
                .await
        }
        None => Ok(()),
    }
}

/// `new MetadataRefreshOptions(…) { ForceSave = true }`, the refresh a
/// membership change queues.
fn force_save_refresh() -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        force_save: true,
        ..MetadataRefreshOptions::default()
    }
}

/// `UpdateRatingToItems` (`BaseItem.cs:2974`) for a box set or playlist whose
/// membership just changed, then the save upstream always makes, which
/// re-derives the parental score. A locked `OfficialRating` keeps its rating
/// on every path: upstream's direct call skips that check, and a field lock a
/// membership change silently overrides is not ported.
async fn update_rating_to_items(
    library_manager: &dyn LibraryManager,
    visibility: Option<&crate::item_visibility::ItemVisibility>,
    container: Uuid,
    how: Rerate,
) -> Result<(), ServiceError> {
    let Some(visibility) = visibility else {
        return Ok(());
    };
    let rerate = async {
        // Only a box set or playlist takes its members' rating. Every caller
        // has checked the kind already; this keeps any new one honest.
        let Some(mut row) = library_manager
            .get_item_by_id(container)
            .await?
            .filter(|row| {
                matches!(
                    BaseItemKind::from_stored_type_name(&row.type_),
                    Some(BaseItemKind::BoxSet | BaseItemKind::Playlist)
                )
            })
        else {
            return Ok(());
        };
        let rating_locked = library_manager
            .get_locked_fields_batch(&[container])
            .await?
            .get(&container)
            .is_some_and(|fields| {
                fields.contains(&ferrofin_model::entities::MetadataField::OfficialRating)
            });
        let keeps_rating = rating_locked || (how == Rerate::Refresh && row.is_locked);
        if !keeps_rating {
            row.official_rating = visibility.rating_to_items(&row).await?;
        }
        // The rating columns alone: the rest of `row` is as read above, and
        // saving it whole would undo a share change made to `Data` since.
        library_manager.update_item_rating(row).await
    };
    match rerate.await {
        Err(error) if how == Rerate::Refresh => {
            tracing::warn!(
                item_id = %container,
                %error,
                "could not re-rate a container from its members"
            );
            Ok(())
        }
        result => result,
    }
}

/// The row `id` names when it is an item of `kind`, else `None`: upstream's
/// `GetItemById(id) is not BoxSet` / `is not Playlist` test, which every
/// collection and playlist membership change makes before it touches anything.
/// Without it those writes reach any item the caller names — a movie's name
/// and its version links included.
async fn item_of_kind(
    library_manager: &dyn LibraryManager,
    id: Uuid,
    kind: BaseItemKind,
) -> Result<Option<BaseItemEntity>, ServiceError> {
    Ok(library_manager
        .get_item_by_id(id)
        .await?
        .filter(|row| BaseItemKind::from_stored_type_name(&row.type_) == Some(kind)))
}

/// The query `Playlist.GetPlaylistItems` lists a folder's playlist entries
/// with (`Playlist.cs:217-228`): every non-folder audio or video item under
/// it that `user` may see, through the folder's own `GetItemsInternal` — a
/// series by its key, in name order and never virtual (`Series.cs:256-283`),
/// a season through its episodes, a box set or playlist through its links.
fn folder_items_query(
    id: Uuid,
    kind: BaseItemKind,
    folder: &BaseItemEntity,
    user: Option<UserEntity>,
) -> InternalItemsQuery {
    let mut query = InternalItemsQuery {
        recursive: true,
        is_folder: Some(false),
        media_types: vec![MediaType::Audio, MediaType::Video],
        enable_total_record_count: false,
        ..InternalItemsQuery::default()
    };
    if let Some(user) = user {
        query.set_user(user);
    }
    if kind == BaseItemKind::Series {
        query.series_presentation_unique_key =
            Some(crate::library_manager::episode_presentation_key(folder));
        query.include_item_types = vec![BaseItemKind::Episode, BaseItemKind::Season];
        query.order_by = vec![(ItemSortBy::SortName, SortOrder::Ascending)];
        query.is_virtual_item = Some(false);
    } else {
        query.set_parent(Some((id, kind)));
        query.apply_folder_display_options = true;
    }
    query
}

/// Upstream's `GetUserById(Guid.Empty)` throw (`UserManager.cs:123-128`, a
/// 400 through `ExceptionMiddleware`): `CreatePlaylist` and every
/// `GetPlaylists`/`GetPlaylistForUser` lookup make it, so an API key that
/// names no user gets neither an unowned playlist nor a look at one.
fn require_user(user_id: Uuid) -> Result<(), ServiceError> {
    if user_id.is_nil() {
        return Err(ServiceError::invalid_input("Guid can't be empty"));
    }
    Ok(())
}

/// Decides the caller's [`PlaylistAccess`] from the three joined access columns
/// (C# `Playlist.IsVisible` plus the owner/`CanEdit` split the controller checks
/// derive from it). `None` means invisible — callers report `NotFound`.
///
/// Shared by the access query and the joined items read so both paths can never
/// drift apart.
fn decide_access(
    owner: Option<&str>,
    open_access: Option<i64>,
    can_edit: Option<i64>,
    user_id: Uuid,
) -> Option<PlaylistAccess> {
    // `open_access` is NULL only when the meta row is absent — a legacy
    // playlist predating owner tracking: every caller is owner-equivalent.
    let Some(open_access) = open_access else {
        return Some(PlaylistAccess {
            level: PlaylistAccessLevel::Owner,
            open_access: false,
        });
    };
    let open_access = open_access != 0;
    // A NULL owner in an existing meta row (an older build's user-less
    // API-key create, or a `Data` naming no owner) means no user owns it, so
    // — like a legacy row — every caller is owner-equivalent.
    // Parse-compare the stored owner GUID so rows written in either case
    // (legacy lowercase, canonical uppercase) still match. The empty GUID is
    // nobody's: a playlist the library scan found is owned by it
    // (`PlaylistResolver`). No caller here is the empty GUID — the manager
    // refuses one first ([`require_user`]) — so the guard is defensive.
    let owner_is_caller =
        !user_id.is_nil() && owner.and_then(|o| Uuid::parse_str(o).ok()) == Some(user_id);
    let level = if owner.is_none() || owner_is_caller {
        PlaylistAccessLevel::Owner
    } else if let Some(can_edit) = can_edit {
        if can_edit != 0 {
            PlaylistAccessLevel::CanEdit
        } else {
            PlaylistAccessLevel::Read
        }
    } else if open_access {
        PlaylistAccessLevel::Read
    } else {
        return None;
    };
    Some(PlaylistAccess { level, open_access })
}

/// The id derivation this database uses, and the user-root row a provisioned
/// container hangs off.
///
/// Both come from the database rather than a default: the derivation is
/// whatever `FerrofinMeta.item_id_derivation` says (an adopted or fresh
/// database is Jellyfin-parity, a pre-parity Ferrofin one is grandfathered),
/// and using the wrong one gives the container an id Jellyfin will not
/// recognise as its own.
async fn container_identity(
    db: &Database,
    paths: &Arc<dyn ServerApplicationPaths>,
) -> Result<(crate::item_type_lookup::IdDerivation, Option<Uuid>), ServiceError> {
    let stored = db
        .meta_get("item_id_derivation")
        .await
        .map_err(|e| ServiceError::Backend(e.to_string()))?;
    let mode = crate::item_type_lookup::IdDerivation::from_meta(
        stored.as_deref(),
        Some(paths.program_data_path()),
    );
    let root =
        crate::item_type_lookup::user_root_folder_id(&mode, &paths.default_user_views_path());
    Ok((mode, root))
}

/// The `Data` blob the auto-provisioned "Collections" library carries, and the
/// only key of it Ferrofin reads or writes.
///
/// 10.11.8 keeps a `CollectionFolder`'s `CollectionType` in that column
/// (`DtoService.AttachBasicFields` reads `IHasCollectionType.CollectionType`,
/// `CollectionFolder.cs:32`), and its own row for this library says
/// `"CollectionType":"boxsets"` because `CollectionManager.EnsureLibraryFolder`
/// creates it with `CollectionTypeOptions.boxsets`. Only written over an EMPTY
/// `Data` column — see `item_persistence_service::backfill_container_data` for
/// why an adopted database's richer blob must survive untouched.
const COLLECTIONS_FOLDER_DATA: &str = r#"{"CollectionType":"boxsets"}"#;

/// The "Collections" library a created box set belongs to, adopting any
/// orphans a previous version left behind.
///
/// `name` is the name a new library takes; an existing one keeps its own.
async fn collections_folder(
    db: &Database,
    paths: &Arc<dyn ServerApplicationPaths>,
    name: &str,
) -> Result<Option<Uuid>, ServiceError> {
    let path = format!("{}/collections", paths.data_path());
    let (mode, root) = container_identity(db, paths).await?;
    let container = crate::item_persistence_service::ensure_container(
        db,
        BaseItemKind::CollectionFolder,
        name,
        &path,
        &mode,
        root,
        // Upstream provisions this library through
        // `AddVirtualFolder(name, CollectionTypeOptions.boxsets, …)`
        // (v10.11.8 CollectionManager.cs:81-109), which is what puts
        // `"CollectionType":"boxsets"` in the row's `Data` blob. Without it the
        // folder went out with a null `CollectionType` everywhere it is listed.
        Some(COLLECTIONS_FOLDER_DATA),
    )
    .await?;
    if let Some(id) = container {
        crate::item_persistence_service::adopt_orphans(db, BaseItemKind::BoxSet, id).await?;
    }
    Ok(container)
}

/// The playlists folder a created playlist belongs to, adopting any orphans a
/// previous version left behind, and the id derivation its playlists are
/// keyed by.
///
/// A lookup only, as `PlaylistManager.GetPlaylistsFolder` is
/// (v12.2 PlaylistManager.cs:566-572). The folder is startup's to make, and to
/// keep at the data directory when that moved
/// ([`crate::AggregateFolderStore::ensure`], `LibraryManager.CreateRootFolder`
/// at LibraryManager.cs:1101-1150, under the aggregate). Making it here as
/// well disagreed with that rule: after a data-directory move this missed
/// the row by its path and inserted its id a second time (gl-910).
///
/// # Errors
///
/// [`ServiceError::InvalidInput`] (`parentFolder`) when there is no playlists
/// folder: `CreatePlaylist` throws `ArgumentException(nameof(parentFolder))`
/// (PlaylistManager.cs:84-88), a 400.
async fn playlists_folder(
    db: &Database,
    paths: &Arc<dyn ServerApplicationPaths>,
) -> Result<(Uuid, crate::item_type_lookup::IdDerivation), ServiceError> {
    // `PlaylistsFolder`, not `ManualPlaylistsFolder`: 10.11.8 has no class of
    // the latter name — it is only `PlaylistsFolder.GetClientTypeName()` — and
    // the row an adopted database carries is stored as
    // `…Playlists.PlaylistsFolder`. Asking for the other one would never find
    // Jellyfin's folder.
    let path = format!("{}/playlists", paths.data_path());
    let (mode, _root) = container_identity(db, paths).await?;
    // By path first: a folder already at the directory under an id of its own
    // (an earlier Ferrofin's `ManualPlaylistsFolder`) is the one in use, and
    // startup provisions no second one beside it. Then by the id
    // `CreateRootFolder` derives, from the program-data-relative
    // `data\playlists`, which a moved data directory keeps where the path does
    // not; upstream finds the folder by its type, wherever it is.
    let found = match crate::item_persistence_service::container_at(db, &path).await? {
        Some(id) => Some(id),
        None => match crate::item_type_lookup::derive_item_id_with(
            &mode,
            BaseItemKind::PlaylistsFolder,
            &path,
        ) {
            Some(id) if crate::item_persistence_service::row_exists(db, id).await? => Some(id),
            _ => None,
        },
    };
    let Some(folder) = found else {
        return Err(ServiceError::invalid_input("parentFolder"));
    };
    crate::item_persistence_service::adopt_orphans(db, BaseItemKind::Playlist, folder).await?;
    Ok((folder, mode))
}

/// Makes the folder `PlaylistManager.CreatePlaylist` gives a new playlist
/// named `name` (`PlaylistManager.cs:83,130-137`) and returns its path:
/// `Path.Combine(playlists, GetValidFilename(name))`, with `1` appended
/// while that is taken (`GetTargetPath`).
///
/// The creation itself tells a taken path, not an existence check before
/// it, so two creates of one name never share a folder. An empty name gets
/// a folder inside `playlists` (`playlists/1`): upstream's `Path.Combine`
/// of an empty name is the playlists folder itself, which `GetTargetPath`
/// turns into `playlists1`, beside the folder its rescan walks.
///
/// # Errors
/// The I/O error that kept the folder from being made.
async fn new_playlist_folder(playlists: &str, name: &str) -> std::io::Result<String> {
    tokio::fs::create_dir_all(playlists).await?;
    let mut path = std::path::Path::new(playlists)
        .join(crate::item_type_lookup::get_valid_filename(name))
        .to_string_lossy()
        .into_owned();
    loop {
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => path.push('1'),
            Err(e) => return Err(e),
        }
    }
}

/// [`new_playlist_folder`] and the id `Folder.AddChild` derives from it
/// under `mode`. A row already at that id (a playlist whose folder went
/// missing) keeps the folder, made again for it, and the next name is
/// tried, so the id is always free.
///
/// # Errors
/// The I/O error that kept a folder from being made, or a failed lookup.
pub(crate) async fn new_playlist_folder_and_id(
    db: &Database,
    mode: &crate::item_type_lookup::IdDerivation,
    playlists: &str,
    name: &str,
) -> Result<(String, Uuid), ServiceError> {
    loop {
        let path = new_playlist_folder(playlists, name)
            .await
            .map_err(|e| ServiceError::backend(format!("create the playlist folder: {e}")))?;
        let id = crate::item_type_lookup::derive_item_id_with(mode, BaseItemKind::Playlist, &path)
            .ok_or_else(|| ServiceError::backend("no stored type name for Playlist"))?;
        if !crate::item_persistence_service::row_exists(db, id).await? {
            return Ok((path, id));
        }
    }
}

/// The folder C# `CollectionManager.CreateCollectionAsync` gives a new box set
/// named `name`, `Path.Combine(collections, GetValidFilename(name) +
/// " [boxset]")`, made if absent (`Directory.CreateDirectory`), and the id
/// `Folder.AddChild` derives from it under `mode`.
///
/// Upstream reuses the folder, and so the id, of a box set already there by
/// that name. Here a folder whose id a row already owns is passed over for
/// `<name>1 [boxset]`, `<name>2 [boxset]`, …, so a second box set of the same
/// name is a box set of its own.
///
/// # Errors
/// The I/O error that kept the folder from being made, or a failed lookup.
async fn new_box_set_folder_and_id(
    db: &Database,
    mode: &crate::item_type_lookup::IdDerivation,
    collections: &str,
    name: &str,
) -> Result<(String, Uuid), ServiceError> {
    let base = crate::item_type_lookup::get_valid_filename(name);
    for attempt in 0_u32.. {
        let folder = if attempt == 0 {
            format!("{base} [boxset]")
        } else {
            format!("{base}{attempt} [boxset]")
        };
        let path = std::path::Path::new(collections)
            .join(folder)
            .to_string_lossy()
            .into_owned();
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|e| ServiceError::backend(format!("create the box set folder: {e}")))?;
        let id = crate::item_type_lookup::derive_item_id_with(mode, BaseItemKind::BoxSet, &path)
            .ok_or_else(|| ServiceError::backend("no stored type name for BoxSet"))?;
        if !crate::item_persistence_service::row_exists(db, id).await? {
            return Ok((path, id));
        }
    }
    Err(ServiceError::backend("no free box set folder"))
}

/// Linked membership and local alternates are read in bounded batches rather
/// than one SQLite round trip per movie in an unpaged plain-folder browse.
async fn linked_children_for_parents(
    db: &Database,
    parents: &[Uuid],
    child_type: Option<i32>,
) -> Result<std::collections::HashMap<Uuid, std::collections::HashSet<Uuid>>, ServiceError> {
    let mut result = std::collections::HashMap::new();
    for parents in parents.chunks(512) {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            r#"SELECT "ParentId", "ChildId" FROM "LinkedChildren" WHERE "ParentId" IN ("#,
        );
        let mut separated = query.separated(", ");
        for parent in parents {
            separated.push_bind(guid_to_db(*parent));
        }
        query.push(")");
        if let Some(kind) = child_type {
            query.push(r#" AND "ChildType" = "#).push_bind(kind);
        }
        let rows: Vec<(String, String)> = query
            .build_query_as()
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
        for (parent, child) in rows {
            if let (Ok(parent), Ok(child)) = (Uuid::parse_str(&parent), Uuid::parse_str(&child)) {
                result
                    .entry(parent)
                    .or_insert_with(std::collections::HashSet::new)
                    .insert(child);
            }
        }
    }
    Ok(result)
}

/// The concrete (minimal) collection (box-set) manager.
#[derive(Clone)]
pub struct FerrofinCollectionManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
    linked_children: Arc<dyn LinkedChildrenService>,
    paths: Arc<dyn ServerApplicationPaths>,
    /// Scores the members' ratings a box set takes
    /// ([`update_rating_to_items`]). `None` in unit tests: the rating stays.
    visibility: Option<Arc<crate::item_visibility::ItemVisibility>>,
    /// Queues the box set's refresh after a membership change
    /// ([`queue_refresh`]). `None` in unit tests: nothing is queued.
    providers: Option<Arc<dyn ProviderManager>>,
    /// Brackets the folder a new box set is made in, as `CollectionManager.CreateCollectionAsync` does
    /// (`ReportFileSystemChangeBeginning`/`Complete`). `None` in unit tests.
    self_writes: Option<Arc<dyn SelfWriteGuard>>,
    /// Names a new Collections library in the server UI culture
    /// ([`Self::collections_name`]). `None` in unit tests: "Collections".
    localization: Option<Arc<dyn ferrofin_traits::localization::LocalizationManager>>,
}

impl std::fmt::Debug for FerrofinCollectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinCollectionManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinCollectionManager {
    /// Creates a collection manager from its injected collaborators.
    #[must_use]
    pub fn new(
        db: Database,
        library_manager: Arc<dyn LibraryManager>,
        linked_children: Arc<dyn LinkedChildrenService>,
        paths: Arc<dyn ServerApplicationPaths>,
    ) -> Self {
        Self {
            db,
            library_manager,
            linked_children,
            paths,
            visibility: None,
            providers: None,
            self_writes: None,
            localization: None,
        }
    }

    /// Names a new Collections library in the server UI culture. Called once
    /// by the composition root.
    #[must_use]
    pub fn with_localization(
        mut self,
        localization: Arc<dyn ferrofin_traits::localization::LocalizationManager>,
    ) -> Self {
        self.localization = Some(localization);
        self
    }

    /// The name a new Collections library takes:
    /// `GetServerLocalizedString("Collections")` (v12.2
    /// `CollectionManager.EnsureLibraryFolder`), whatever the requesting
    /// client's language. Upstream never renames the library afterwards.
    fn collections_name(&self) -> String {
        self.localization.as_ref().map_or_else(
            || "Collections".to_owned(),
            |localization| localization.get_server_localized_string("Collections"),
        )
    }

    /// Brackets each new box set's folder on `guard`, so the library monitor
    /// takes it for the server's own write. Called once by the composition
    /// root.
    #[must_use]
    pub fn with_self_writes(mut self, guard: Arc<dyn SelfWriteGuard>) -> Self {
        self.self_writes = Some(guard);
        self
    }

    /// Queues a box set's provider refresh after it is created or its members
    /// change, as `CollectionManager` does. Called once by the composition
    /// root.
    #[must_use]
    pub fn with_providers(mut self, providers: Arc<dyn ProviderManager>) -> Self {
        self.providers = Some(providers);
        self
    }

    /// Lets a box set take its members' rating when they change. Called once
    /// by the composition root.
    #[must_use]
    pub fn with_visibility(
        mut self,
        visibility: Arc<crate::item_visibility::ItemVisibility>,
    ) -> Self {
        self.visibility = Some(visibility);
        self
    }

    async fn update_rating_to_items(
        &self,
        collection_id: Uuid,
        how: Rerate,
    ) -> Result<(), ServiceError> {
        update_rating_to_items(
            self.library_manager.as_ref(),
            self.visibility.as_deref(),
            collection_id,
            how,
        )
        .await
    }

    /// `AddToCollectionAsync`/`RemoveFromCollectionAsync`: an id that is not
    /// a box set's is an `ArgumentException` (→ `400`).
    async fn require_box_set(&self, id: Uuid) -> Result<(), ServiceError> {
        item_of_kind(self.library_manager.as_ref(), id, BaseItemKind::BoxSet)
            .await?
            .map(drop)
            .ok_or_else(|| {
                ServiceError::invalid_input(format!("no collection exists with id {id}"))
            })
    }
}

#[async_trait]
impl CollectionManager for FerrofinCollectionManager {
    async fn create_collection(
        &self,
        options: &CollectionCreationOptions,
    ) -> Result<BaseItemEntity, ServiceError> {
        let parent = collections_folder(&self.db, &self.paths, &self.collections_name()).await?;
        let (mode, _) = container_identity(&self.db, &self.paths).await?;
        // `GetCollectionsFolderPath()`: `{data}/collections`.
        let collections = format!("{}/collections", self.paths.data_path());
        let (path, id) =
            new_box_set_folder_and_id(&self.db, &mode, &collections, &options.name).await?;
        // `ReportFileSystemChangeBeginning(path)` around the whole create.
        let _write = SelfWrite::begin(self.self_writes.as_ref(), &path);
        // The row and its path commit together, so no box set is ever
        // stored without the folder it was made with.
        let written = async {
            let mut tx = self.db.writer().begin().await.map_err(db_err)?;
            crate::item_persistence_service::insert_named_item_in(
                &mut tx,
                id,
                BaseItemKind::BoxSet,
                &options.name,
                true,
                parent,
                options.is_locked,
            )
            .await?;
            crate::item_persistence_service::set_item_path_in(&mut tx, id, &path)
                .await
                .map_err(db_err)?;
            tx.commit().await.map_err(db_err)
        }
        .await;
        if let Err(err) = written {
            // A folder with no row would come back as an empty box set on a
            // Jellyfin rescan. `remove_dir` leaves a folder that has content.
            if let Err(e) = tokio::fs::remove_dir(&path).await {
                tracing::warn!(%e, path = %path, "could not remove an unused box set folder");
            }
            return Err(err);
        }
        let row = self
            .library_manager
            .get_item_by_id(id)
            .await?
            .ok_or_else(|| ServiceError::backend(format!("box set {id} vanished after create")))?;
        for item_id in &options.item_id_list {
            self.linked_children
                .upsert_linked_child(id, *item_id, LINKED_CHILD_MANUAL)
                .await?;
        }
        if options.item_id_list.is_empty() {
            // `CreateCollectionAsync` with no items: a default refresh.
            queue_refresh(
                self.providers.as_deref(),
                id,
                MetadataRefreshOptions::default(),
            )
            .await?;
            return Ok(row);
        }
        self.update_rating_to_items(id, Rerate::Direct).await?;
        // `AddToCollectionAsync` with the `FullRefresh` options create passes
        // ("the initial adding of items is going to create a local metadata
        // file"), which it then marks `ForceSave`.
        queue_refresh(
            self.providers.as_deref(),
            id,
            MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                ..force_save_refresh()
            },
        )
        .await?;
        Ok(self
            .library_manager
            .get_item_by_id(id)
            .await?
            .unwrap_or(row))
    }

    async fn add_to_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        self.require_box_set(collection_id).await?;
        let current = self
            .linked_children
            .get_linked_children_ids(collection_id, None)
            .await?;
        for item_id in item_ids {
            self.linked_children
                .upsert_linked_child(collection_id, *item_id, LINKED_CHILD_MANUAL)
                .await?;
        }
        // `AddToCollectionAsync` re-rates and queues its refresh only when an
        // item was new.
        if item_ids.iter().any(|id| !current.contains(id)) {
            self.update_rating_to_items(collection_id, Rerate::Direct)
                .await?;
            queue_refresh(
                self.providers.as_deref(),
                collection_id,
                force_save_refresh(),
            )
            .await?;
        }
        Ok(())
    }

    async fn remove_from_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        self.require_box_set(collection_id).await?;
        for item_id in item_ids {
            sqlx::query(
                r#"DELETE FROM "LinkedChildren"
                   WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
            )
            .bind(guid_to_db(collection_id))
            .bind(guid_to_db(*item_id))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        }
        self.update_rating_to_items(collection_id, Rerate::Refresh)
            .await?;
        // `RemoveFromCollectionAsync` queues its refresh whatever it removed.
        queue_refresh(
            self.providers.as_deref(),
            collection_id,
            force_save_refresh(),
        )
        .await
    }

    async fn get_collections_containing_item(
        &self,
        user: &ferrofin_db::entities::users::UserEntity,
        item_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // C#: the manual-link parents of type BoxSet, each resolved through
        // `GetItemById<BoxSet>(parentId, user)` — hidden ones drop out.
        if item_id.is_nil() {
            return Ok(Vec::new());
        }
        let parent_ids = self
            .linked_children
            .get_manual_linked_parent_ids(item_id, Some(BaseItemKind::BoxSet))
            .await?;
        let mut out = Vec::new();
        for parent_id in parent_ids {
            if let Some(row) = self
                .library_manager
                .get_item_by_id_for_user(parent_id, Some(user))
                .await?
            {
                out.push(row);
            }
        }
        Ok(out)
    }

    async fn collapse_items_within_box_sets(
        &self,
        items: &[BaseItemEntity],
        user: Option<&ferrofin_db::entities::users::UserEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        use ferrofin_traits::options::InternalItemsQuery;
        use std::collections::HashSet;
        let collections = if let Some(folder) = self.get_collections_folder(false).await? {
            let query = InternalItemsQuery {
                user: user.cloned(),
                parent_id: Uuid::parse_str(&folder.id)
                    .map_err(|error| ServiceError::backend(error.to_string()))?,
                include_item_types: vec![BaseItemKind::BoxSet],
                group_by_presentation_unique_key: false,
                ..InternalItemsQuery::default()
            };
            let rows = self.library_manager.query_items(&query).await?.items;
            if let Some(user) = user {
                self.library_manager
                    .filter_visible_items(rows, user)
                    .await?
            } else {
                rows
            }
        } else {
            Vec::new()
        };
        let collection_ids: Vec<_> = collections
            .iter()
            .filter_map(|item| Uuid::parse_str(&item.id).ok())
            .collect();
        let movie_ids: Vec<_> = items
            .iter()
            .filter(|item| item.type_.rsplit('.').next() == Some("Movie"))
            .filter_map(|item| Uuid::parse_str(&item.id).ok())
            .collect();
        // ContainsLinkedChildByItemId checks all linked child kinds; alternate
        // suppression consults only GetLocalAlternateVersionIds (type 2).
        let (members, alternates) = tokio::try_join!(
            linked_children_for_parents(&self.db, &collection_ids, None),
            linked_children_for_parents(&self.db, &movie_ids, Some(2)),
        )?;
        let mut results = Vec::new();
        let mut seen = HashSet::new();
        for item in items {
            let id = Uuid::parse_str(&item.id)
                .map_err(|error| ServiceError::backend(error.to_string()))?;
            let kind = item.type_.rsplit('.').next().unwrap_or(&item.type_);
            if matches!(kind, "Movie" | "Series") {
                let mut in_collection = false;
                for collection in &collections {
                    let collection_id = Uuid::parse_str(&collection.id)
                        .map_err(|error| ServiceError::backend(error.to_string()))?;
                    if !members
                        .get(&collection_id)
                        .is_some_and(|children| children.contains(&id))
                    {
                        continue;
                    }
                    in_collection = true;
                    if seen.insert(collection_id) {
                        results.push(collection.clone());
                    }
                }
                if in_collection {
                    continue;
                }
                if kind == "Movie"
                    && alternates
                        .get(&id)
                        .is_some_and(|children| children.iter().any(|child| seen.contains(child)))
                {
                    continue;
                }
            }
            if seen.insert(id) {
                results.push(item.clone());
            }
        }
        Ok(results)
    }

    async fn get_collections_folder(
        &self,
        create_if_needed: bool,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        // Port of `CollectionManager.EnsureLibraryFolder`
        // (v10.11.8 Emby.Server.Implementations/Collections/CollectionManager.cs
        // :81-109): look the library up BY PATH (then by its derived id, for a
        // data directory that moved), and provision it — directory,
        // row, boxsets collection type and all — only when the caller asked for
        // it. This used to answer `Ok(None)` unconditionally under a "documented
        // deferral" comment; the tree it was said to need is the same
        // `ensure_container` the create path has been using all along.
        let path = format!("{}/collections", self.paths.data_path());
        let id = if create_if_needed {
            collections_folder(&self.db, &self.paths, &self.collections_name()).await?
        } else if let Some(id) =
            crate::item_persistence_service::container_at(&self.db, &path).await?
        {
            Some(id)
        } else {
            // A data directory that moved left the row at its old path (gl-922).
            let (mode, _) = container_identity(&self.db, &self.paths).await?;
            crate::item_persistence_service::moved_container(
                &self.db,
                BaseItemKind::CollectionFolder,
                &path,
                &mode,
            )
            .await?
        };
        let Some(id) = id else {
            return Ok(None);
        };
        crate::item_persistence_service::container_row(&self.db, id).await
    }
}

/// The concrete (minimal) playlist manager.
#[derive(Clone)]
pub struct FerrofinPlaylistManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
    items: Arc<dyn ItemRepository>,
    paths: Arc<dyn ServerApplicationPaths>,
    /// `Playlist.IsVisible` beyond the owner/share check
    /// ([`Self::require_visible`]) and member visibility for a move, plus the
    /// users they read. `None` in unit tests: the owner/share check alone then
    /// decides and every member counts as visible.
    visibility: Option<(
        Arc<crate::item_visibility::ItemVisibility>,
        Arc<dyn UserManager>,
    )>,
    /// Held across the read-modify-write of an add, remove, move or update
    /// (the refresh they queue runs after it is released): each reads the
    /// entries and writes them back, so two overlapping edits would both
    /// start from the same state and lose one of the changes (parallel
    /// per-track adds do exactly that). Share writes made outside an update
    /// do not take it (`update_playlist` writes its shares under the guard):
    /// the re-rate after an edit saves only its rating columns, never
    /// `Data`, and every `Data` sync reads and writes in one writer
    /// transaction, so neither can put an older `Shares` back.
    // ponytail: one lock for every playlist; key it by playlist if edits ever contend.
    entry_edits: Arc<tokio::sync::Mutex<()>>,
    /// Queues the playlist's refresh after its entries change
    /// ([`queue_refresh`]). `None` in unit tests: nothing is queued.
    providers: Option<Arc<dyn ProviderManager>>,
    /// Brackets the folder a new playlist is made in, as `PlaylistManager.CreatePlaylist` does
    /// (`ReportFileSystemChangeBeginning`/`Complete`). `None` in unit tests.
    self_writes: Option<Arc<dyn SelfWriteGuard>>,
    /// Announces each committed change of a playlist ([`Self::announce_added`],
    /// [`Self::announce_updated`]). `None` in unit tests: nothing is announced.
    persistence: Option<Arc<dyn ItemPersistenceService>>,
}

impl std::fmt::Debug for FerrofinPlaylistManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinPlaylistManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinPlaylistManager {
    /// Creates a playlist manager from its injected collaborators.
    #[must_use]
    pub fn new(
        db: Database,
        library_manager: Arc<dyn LibraryManager>,
        items: Arc<dyn ItemRepository>,
        paths: Arc<dyn ServerApplicationPaths>,
    ) -> Self {
        Self {
            db,
            library_manager,
            items,
            paths,
            visibility: None,
            entry_edits: Arc::default(),
            providers: None,
            self_writes: None,
            persistence: None,
        }
    }

    /// Announces each committed playlist change to the library-changed
    /// notifier through `persistence`, as upstream's `UpdatePlaylistInternal`
    /// and `Folder.AddChild` do. Called once by the composition root.
    #[must_use]
    pub fn with_item_persistence(mut self, persistence: Arc<dyn ItemPersistenceService>) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// `Folder.AddChild` → `LibraryManager.CreateItem`'s `ItemAdded` for a new
    /// playlist. The create is committed by now, so a failure is logged, not
    /// returned.
    async fn announce_added(&self, playlist_id: Uuid) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        if let Err(err) = persistence.report_item_added(playlist_id).await {
            tracing::warn!(item_id = %playlist_id, %err, "could not announce a new playlist");
        }
    }

    /// `UpdatePlaylistInternal` → `UpdateToRepositoryAsync(MetadataEdit)`:
    /// stamps the playlist's `DateLastSaved` (its Etag moves) and announces
    /// `ItemUpdated`. The change is committed by now, so a failure is logged,
    /// not returned.
    async fn announce_updated(&self, playlist_id: Uuid) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        if let Err(err) = persistence.touch_item_updated(playlist_id).await {
            tracing::warn!(item_id = %playlist_id, %err, "could not announce a playlist change");
        }
    }

    /// Brackets each new playlist's folder on `guard`, so the library monitor
    /// takes it for the server's own write. Called once by the composition
    /// root.
    #[must_use]
    pub fn with_self_writes(mut self, guard: Arc<dyn SelfWriteGuard>) -> Self {
        self.self_writes = Some(guard);
        self
    }

    /// Queues a playlist's provider refresh after entries are added or
    /// removed, as `PlaylistManager` does. Called once by the composition
    /// root.
    #[must_use]
    pub fn with_providers(mut self, providers: Arc<dyn ProviderManager>) -> Self {
        self.providers = Some(providers);
        self
    }

    /// Adds `Playlist.IsVisible`'s parental half to every access decision.
    /// Called once by the composition root.
    #[must_use]
    pub fn with_visibility(
        mut self,
        visibility: Arc<crate::item_visibility::ItemVisibility>,
        users: Arc<dyn UserManager>,
    ) -> Self {
        self.visibility = Some((visibility, users));
        self
    }

    /// The rating refresh after a membership change ([`update_rating_to_items`]).
    async fn update_rating_to_items(&self, playlist_id: Uuid) -> Result<(), ServiceError> {
        update_rating_to_items(
            self.library_manager.as_ref(),
            self.visibility
                .as_ref()
                .map(|(visibility, _)| visibility.as_ref()),
            playlist_id,
            Rerate::Refresh,
        )
        .await
    }

    /// The visibility evaluator and the caller, when the evaluator is wired.
    async fn visibility_user(
        &self,
        user_id: Uuid,
    ) -> Result<Option<(&crate::item_visibility::ItemVisibility, UserEntity)>, ServiceError> {
        let Some((visibility, users)) = &self.visibility else {
            return Ok(None);
        };
        Ok(users
            .get_user_by_id(user_id)
            .await?
            .map(|user| (visibility.as_ref(), user)))
    }

    /// The entries an add stores for `ids` — `GetPlaylistItems(ids, user)
    /// .Where(i => i.SupportsAddingToPlaylist)` (`PlaylistManager.cs:194-224`,
    /// `Playlist.GetPlaylistItems`, `Playlist.cs:173-231`). An id naming no
    /// item is dropped, and so is an item the user may not see. A music genre
    /// or artist becomes its tracks (album artist, album, then name order),
    /// any other folder its non-folder audio and video
    /// ([`folder_items_query`]), and a kind that cannot be added — a photo, a
    /// book, a person — is dropped. A nil `user_id` (an API key) sees
    /// everything.
    async fn playlist_items(&self, ids: &[Uuid], user_id: Uuid) -> Result<Vec<Uuid>, ServiceError> {
        let user = if user_id.is_nil() {
            None
        } else {
            crate::item_repository::query_user(&self.db, user_id).await?
        };
        let by_id: std::collections::HashMap<Uuid, BaseItemEntity> = self
            .items
            .retrieve_items(ids)
            .await?
            .into_iter()
            .filter_map(|row| Some((Uuid::parse_str(&row.id).ok()?, row)))
            .collect();
        // `itemIds.Select(GetItemById).Where(i => i is not null)`: request
        // order, repeats kept.
        let mut inputs: Vec<(Uuid, &BaseItemEntity)> = ids
            .iter()
            .filter_map(|id| by_id.get(id).map(|row| (*id, row)))
            .collect();
        if let (Some(user), Some((visibility, _))) = (&user, &self.visibility) {
            let rows: Vec<BaseItemEntity> = inputs.iter().map(|(_, row)| (*row).clone()).collect();
            let visible = visibility.visible(&rows, user, false).await?;
            inputs = inputs
                .into_iter()
                .zip(visible)
                .filter_map(|(input, visible)| visible.then_some(input))
                .collect();
        }
        let mut items = Vec::new();
        for (id, row) in inputs {
            match kind_from_type_name(&row.type_) {
                Some(kind @ (BaseItemKind::MusicGenre | BaseItemKind::MusicArtist)) => {
                    let mut query = InternalItemsQuery {
                        recursive: true,
                        include_item_types: vec![BaseItemKind::Audio],
                        order_by: vec![
                            (ItemSortBy::AlbumArtist, SortOrder::Ascending),
                            (ItemSortBy::Album, SortOrder::Ascending),
                            (ItemSortBy::SortName, SortOrder::Ascending),
                        ],
                        ..InternalItemsQuery::default()
                    };
                    if let Some(user) = user.clone() {
                        query.set_user(user);
                    }
                    if kind == BaseItemKind::MusicGenre {
                        query.genre_ids = vec![id];
                    } else {
                        query.artist_ids = vec![id];
                    }
                    items.extend(self.library_manager.get_item_list(&query).await?);
                }
                Some(kind) if crate::kinds::is_folder(kind) => {
                    let query = folder_items_query(id, kind, row, user.clone());
                    items.extend(self.library_manager.query_items(&query).await?.items);
                }
                _ => items.push(row.clone()),
            }
        }
        Ok(items
            .into_iter()
            .filter(|row| {
                kind_from_type_name(&row.type_)
                    .is_some_and(crate::kinds::supports_adding_to_playlist)
            })
            .filter_map(|row| Uuid::parse_str(&row.id).ok())
            .collect())
    }

    /// `CreatePlaylist`'s media type when the request names none
    /// (`PlaylistManager.cs:90-127`): the first requested item that answers
    /// decides — its own media type, `Audio` for an artist, album or music
    /// genre, `Video` for a genre, a folder's first playable member's — and
    /// `Audio` when none does. An id naming no item before that is upstream's
    /// `ArgumentException`, thrown before anything is created.
    async fn infer_media_type(&self, ids: &[Uuid]) -> Result<MediaType, ServiceError> {
        for id in ids {
            let item = self
                .library_manager
                .get_item_by_id(*id)
                .await?
                .ok_or_else(|| {
                    ServiceError::invalid_input("No item exists with the supplied Id")
                })?;
            let kind = kind_from_type_name(&item.type_);
            let media_type = match crate::dto_service::parse_media_type(item.media_type.as_deref())
            {
                MediaType::Unknown => match kind {
                    Some(
                        BaseItemKind::MusicArtist
                        | BaseItemKind::MusicAlbum
                        | BaseItemKind::MusicGenre,
                    ) => MediaType::Audio,
                    Some(BaseItemKind::Genre) => MediaType::Video,
                    // `GetRecursiveChildren(i => !i.IsFolder &&
                    // i.SupportsAddingToPlaylist)`, with no user, missing
                    // episodes included. Those members are all audio or
                    // video, so the folder query finds them; it reads them in
                    // name order where upstream walks the tree, which only
                    // differs for a folder holding both.
                    Some(kind) if crate::kinds::is_folder(kind) => {
                        let mut query = folder_items_query(*id, kind, &item, None);
                        query.is_virtual_item = None;
                        query.limit = Some(1);
                        self.library_manager
                            .query_items(&query)
                            .await?
                            .items
                            .first()
                            .map_or(MediaType::Unknown, |member| {
                                crate::dto_service::parse_media_type(member.media_type.as_deref())
                            })
                    }
                    _ => MediaType::Unknown,
                },
                known => known,
            };
            if media_type != MediaType::Unknown {
                return Ok(media_type);
            }
        }
        Ok(MediaType::Audio)
    }

    /// The rest of `Playlist.IsVisible` after [`decide_access`]'s owner/share
    /// answer ([`ItemVisibility::playlist_policy`](crate::item_visibility::ItemVisibility::playlist_policy)):
    /// a user with a maximum parental rating sees a playlist only when it is
    /// empty or a member passes the rating, and a playlist file also answers
    /// to its own rating and tags. Invisible is `NotFound`, as the C#
    /// `GetPlaylistForUser` returns null — and so is an item that is no
    /// playlist, which that lookup never lists, with or without the evaluator.
    async fn require_visible(&self, playlist_id: Uuid, user_id: Uuid) -> Result<(), ServiceError> {
        let playlist = self.require_playlist(playlist_id).await?;
        let Some((visibility, user)) = self.visibility_user(user_id).await? else {
            return Ok(());
        };
        if visibility
            .playlist_policy(&[playlist], &user)
            .await?
            .contains(&false)
        {
            return Err(ServiceError::not_found(format!("playlist {playlist_id}")));
        }
        Ok(())
    }

    /// Which of `entries` are in the caller's member list: a manual link
    /// (`GET /Playlists/{id}/Items` lists only those) to an existing member
    /// the caller sees (`IsVisible(user)`). Every manual entry without the
    /// evaluator.
    async fn visible_entries(
        &self,
        entries: &[Entry],
        user_id: Uuid,
    ) -> Result<Vec<bool>, ServiceError> {
        let manual = |child_type: i64| child_type == i64::from(LINKED_CHILD_MANUAL);
        let Some((visibility, user)) = self.visibility_user(user_id).await? else {
            return Ok(entries.iter().map(|(_, t)| manual(*t)).collect());
        };
        let ids: Vec<Uuid> = entries
            .iter()
            .filter_map(|(child, _)| Uuid::parse_str(child).ok())
            .collect();
        let rows = self.items.retrieve_items(&ids).await?;
        let visible = visibility.visible(&rows, &user, false).await?;
        let by_id: std::collections::HashMap<Uuid, bool> = rows
            .iter()
            .zip(visible)
            .filter_map(|(row, visible)| Some((Uuid::parse_str(&row.id).ok()?, visible)))
            .collect();
        Ok(entries
            .iter()
            .map(|(child, child_type)| {
                manual(*child_type)
                    && Uuid::parse_str(child)
                        .ok()
                        .and_then(|id| by_id.get(&id).copied())
                        .unwrap_or(false)
            })
            .collect())
    }

    /// Loads a playlist row or errors with [`ServiceError::NotFound`]; an item
    /// of any other kind is not found either.
    async fn require_playlist(&self, id: Uuid) -> Result<BaseItemEntity, ServiceError> {
        item_of_kind(self.library_manager.as_ref(), id, BaseItemKind::Playlist)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("playlist {id}")))
    }

    /// The playlist a membership change names. Upstream's
    /// `AddToPlaylistInternal`, `RemoveItemFromPlaylistAsync` and
    /// `MoveItemAsync` throw `ArgumentException` for an id that is no
    /// playlist's. The controllers' `GetPlaylistForUser` gate answers `404`
    /// before that, except for an API key's remove, which skips the gate and
    /// turns the exception into the same `404`.
    async fn require_playlist_argument(&self, id: &str) -> Result<Uuid, ServiceError> {
        let no_playlist =
            || ServiceError::invalid_input(format!("no playlist exists with id {id}"));
        let uuid = Uuid::parse_str(id).map_err(|_| no_playlist())?;
        match self.require_playlist(uuid).await {
            Ok(_) => Ok(uuid),
            Err(ServiceError::NotFound(_)) => Err(no_playlist()),
            Err(error) => Err(error),
        }
    }

    /// Resolves the caller's access to a playlist (C# `Playlist.IsVisible` plus
    /// the owner/`CanEdit` split the controller checks derive from it).
    ///
    /// One LEFT-JOIN query: the base row proves existence, the `FerrofinPlaylists` meta
    /// row carries owner/open-access, and the caller's own `FerrofinPlaylistShares` row
    /// (if any) carries `CanEdit`. The query reads a row of any kind, so it is the
    /// unconditional [`Self::require_visible`] that proves the row is a playlist.
    /// Missing, invisible **or no playlist** → `NotFound`.
    async fn access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<PlaylistAccess, ServiceError> {
        require_user(user_id)?;
        let row: Option<(Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
            r#"SELECT p."OwnerUserId", p."OpenAccess", s."CanEdit"
               FROM "BaseItems" bi
               LEFT JOIN "FerrofinPlaylists" p ON p."PlaylistId" = bi."Id"
               LEFT JOIN "FerrofinPlaylistShares" s ON s."PlaylistId" = bi."Id" AND s."UserId" = ?2
               WHERE bi."Id" = ?1"#,
        )
        .bind(guid_to_db(playlist_id))
        .bind(guid_to_db(user_id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        let not_found = || ServiceError::not_found(format!("playlist {playlist_id}"));
        let (owner, open_access, can_edit) = row.ok_or_else(not_found)?;
        let access = decide_access(owner.as_deref(), open_access, can_edit, user_id)
            .ok_or_else(not_found)?;
        self.require_visible(playlist_id, user_id).await?;
        Ok(access)
    }
}

#[async_trait]
impl PlaylistManager for FerrofinPlaylistManager {
    async fn get_playlist_access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<PlaylistAccess, ServiceError> {
        self.access(playlist_id, user_id).await
    }

    async fn get_playlist_for_user(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<BaseItemEntity, ServiceError> {
        // Visibility gate first: invisible reads as missing (the C#
        // `GetPlaylistForUser` returns null for both).
        self.access(playlist_id, user_id).await?;
        self.require_playlist(playlist_id).await
    }

    async fn create_playlist(
        &self,
        request: &PlaylistCreationRequest,
    ) -> Result<PlaylistCreationResult, ServiceError> {
        let name = request.name.clone().unwrap_or_default();
        // `GetPlaylistsFolder` before the media type is inferred, as upstream.
        let (parent, mode) = playlists_folder(&self.db, &self.paths).await?;
        // An unknown id fails the create before any of the playlist is
        // written, as upstream's does.
        let media_type = match request.media_type {
            Some(media_type) if media_type != MediaType::Unknown => media_type,
            _ => self.infer_media_type(&request.item_id_list).await?,
        };
        require_user(request.user_id)?;
        // Playlist entries are positions, not a set (C# `LinkedChild.Create`
        // per item, no de-duplication).
        let entries: Vec<Entry> = self
            .playlist_items(&request.item_id_list, request.user_id)
            .await?
            .into_iter()
            .map(|item_id| (guid_to_db(item_id), i64::from(LINKED_CHILD_MANUAL)))
            .collect();
        // The enum's serde form IS Jellyfin's JSON string ("Video", …).
        let media_type = serde_json::to_value(media_type)
            .map_err(|e| ServiceError::Backend(format!("serialize MediaType: {e}")))?;
        // Its own folder, and the id `Folder.AddChild` derives from that
        // path: Jellyfin's rescan resolves every folder in the playlists
        // folder to that id, and deletes a playlist row it finds no folder
        // for.
        let playlists = format!("{}/playlists", self.paths.data_path());
        let (path, id) = new_playlist_folder_and_id(&self.db, &mode, &playlists, &name).await?;
        // `ReportFileSystemChangeBeginning(path)` around the whole create.
        let _write = SelfWrite::begin(self.self_writes.as_ref(), &path);
        // One writer transaction: a playlist row committed without its owner
        // row would open to every caller as its owner (`decide_access`), and
        // no boot import could repair it.
        let written = async {
            let mut tx = self.db.writer().begin().await.map_err(db_err)?;
            crate::item_persistence_service::insert_named_item_in(
                &mut tx,
                id,
                BaseItemKind::Playlist,
                &name,
                true,
                Some(parent),
                false,
            )
            .await?;
            crate::item_persistence_service::set_item_path_in(&mut tx, id, &path)
                .await
                .map_err(db_err)?;
            // The ownership meta row (C# `OwnerUserId`/`OpenAccess`).
            sqlx::query(
                r#"INSERT INTO "FerrofinPlaylists" ("PlaylistId", "OwnerUserId", "OpenAccess")
                   VALUES (?1, ?2, ?3)"#,
            )
            .bind(guid_to_db(id))
            .bind(guid_to_db(request.user_id))
            .bind(i64::from(request.public.unwrap_or(false)))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            // Seed the share list from the request (C# `Shares = options.Users`).
            for share in &request.users {
                item_data::share_playlist_in(&mut tx, id, share.user_id, share.can_edit).await?;
            }
            write_entries_in(&mut tx, &guid_to_db(id), &entries).await?;
            crate::item_persistence_service::stamp_playlist_in(
                &mut tx,
                id,
                media_type.as_str(),
                (!entries.is_empty()).then(chrono::Utc::now),
            )
            .await?;
            // Write the owner, open access and shares into BaseItems.Data, which
            // 12.0 still serialises and each boot re-imports.
            item_data::sync_container_data_in(&mut tx, id).await?;
            // `Playlist.PlaylistMediaType`, which Jellyfin reads back from `Data`.
            item_data::set_data_key_in(&mut tx, id, "PlaylistMediaType", media_type).await?;
            tx.commit().await.map_err(db_err)
        }
        .await;
        if let Err(err) = written {
            // A folder with no row would come back as an empty playlist on a
            // Jellyfin rescan.
            if let Err(e) = tokio::fs::remove_dir(&path).await {
                tracing::warn!(%e, path = %path, "could not remove an unused playlist folder");
            }
            return Err(err);
        }
        self.announce_added(id).await;
        // `await playlist.RefreshMetadata(new MetadataRefreshOptions(..) {
        // ForceSave = true })`: the first refresh runs before the create
        // returns, and a failure fails it, as upstream's exception does.
        if let Some(providers) = self.providers.as_deref() {
            providers
                .refresh_single_item(id, &force_save_refresh())
                .await?;
        }
        if !entries.is_empty() {
            self.update_rating_to_items(id).await?;
            // `AddToPlaylistInternal`'s refresh, reached from `CreatePlaylist`.
            queue_refresh(self.providers.as_deref(), id, force_save_refresh()).await?;
        }
        // `playlist.Id.ToString("N", CultureInfo.InvariantCulture)`.
        Ok(PlaylistCreationResult {
            id: id.simple().to_string(),
        })
    }

    async fn update_playlist(&self, request: &PlaylistUpdateRequest) -> Result<(), ServiceError> {
        self.require_playlist(request.id).await?;
        // C# `UpdatePlaylist` (12.0, :608-614): `Ids` REPLACE the entries —
        // `LinkedChildren = []`, then `AddToPlaylistInternal` — so they expand
        // the way an add does, and duplicates survive. The playlist's own id
        // expands after that clear upstream, to nothing. Only the direct id
        // is dropped here: a container that links this playlist still
        // expands its entries from before the update.
        let entries = match &request.ids {
            Some(ids) => Some(
                self.playlist_items(
                    &ids.iter()
                        .copied()
                        .filter(|id| *id != request.id)
                        .collect::<Vec<_>>(),
                    request.user_id,
                )
                .await?
                .into_iter()
                .map(|item_id| (guid_to_db(item_id), i64::from(LINKED_CHILD_MANUAL)))
                .collect::<Vec<Entry>>(),
            ),
            None => None,
        };
        let edit = self.entry_edits.lock().await;
        // C# `UpdatePlaylist` saves the name, shares and open access together
        // (the last `UpdatePlaylistInternal`). Every change here commits with
        // the `Data` sync in one writer transaction, so a failed update
        // applies none of them: a share or open-access change committed
        // without its sync would be undone by the next boot's re-import. A
        // playlist no boot has imported yet is imported first, so its
        // open-access and share changes land on its own rows.
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        item_data::import_playlist_meta_if_missing_in(&mut tx, request.id).await?;
        if let Some(name) = &request.name {
            crate::item_persistence_service::rename_named_item_in(
                &mut tx,
                request.id,
                BaseItemKind::Playlist,
                name,
            )
            .await?;
        }
        if let Some(public) = request.public {
            sqlx::query(
                r#"UPDATE "FerrofinPlaylists" SET "OpenAccess" = ?2 WHERE "PlaylistId" = ?1"#,
            )
            .bind(guid_to_db(request.id))
            .bind(i64::from(public))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        if let Some(users) = &request.users {
            // C# replaces `Shares` wholesale on update.
            sqlx::query(r#"DELETE FROM "FerrofinPlaylistShares" WHERE "PlaylistId" = ?1"#)
                .bind(guid_to_db(request.id))
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            for share in users {
                item_data::share_playlist_in(&mut tx, request.id, share.user_id, share.can_edit)
                    .await?;
            }
        }
        if let Some(entries) = &entries {
            write_entries_in(&mut tx, &guid_to_db(request.id), entries).await?;
            if !entries.is_empty() {
                crate::item_persistence_service::stamp_playlist_in(
                    &mut tx,
                    request.id,
                    None,
                    Some(chrono::Utc::now()),
                )
                .await?;
            }
        }
        item_data::sync_container_data_in(&mut tx, request.id).await?;
        tx.commit().await.map_err(db_err)?;
        self.announce_updated(request.id).await;
        if entries.as_ref().is_some_and(|entries| !entries.is_empty()) {
            self.update_rating_to_items(request.id).await?;
            // `AddToPlaylistInternal`'s refresh, reached from `UpdatePlaylist`,
            // queued after the edit guard is released, so the queue call never
            // runs under it.
            drop(edit);
            queue_refresh(self.providers.as_deref(), request.id, force_save_refresh()).await?;
        }
        Ok(())
    }

    async fn get_playlists(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        require_user(user_id)?;
        let type_name = stored_type_name(BaseItemKind::Playlist)
            .ok_or_else(|| ServiceError::backend("no stored type name for Playlist"))?;
        // Visibility predicate (C# `Playlist.IsVisible`): legacy/unowned rows,
        // open-access, owned by the caller, or shared with the caller.
        let rows = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT bi.* FROM "BaseItems" bi
               LEFT JOIN "FerrofinPlaylists" p ON p."PlaylistId" = bi."Id"
               WHERE bi."Type" = ?1 AND (
                   p."PlaylistId" IS NULL
                   OR p."OwnerUserId" IS NULL
                   OR p."OpenAccess" = 1
                   OR p."OwnerUserId" = ?2
                   OR EXISTS (SELECT 1 FROM "FerrofinPlaylistShares" s
                              WHERE s."PlaylistId" = bi."Id" AND s."UserId" = ?2))
               ORDER BY bi."Name""#,
        )
        .bind(type_name)
        .bind(guid_to_db(user_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let Some((visibility, user)) = self.visibility_user(user_id).await? else {
            return Ok(rows);
        };
        // `GetPlaylists(userId)`: `.Where(p => p.IsVisible(user))`, whose
        // parental and file-playlist halves SQL cannot answer.
        let allowed = visibility.playlist_policy(&rows, &user).await?;
        Ok(rows
            .into_iter()
            .zip(allowed)
            .filter_map(|(row, allowed)| allowed.then_some(row))
            .collect())
    }

    async fn get_playlist_items(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        require_user(user_id)?;
        // One round-trip through the repository seam: the member rows already
        // in link order (the same ordering `get_linked_children_ids` applies),
        // each carrying the caller's access columns, so the visibility gate
        // costs no extra query. The C# `Forbid()` branch is unreachable here
        // because visible ⇔ permitted-to-read.
        let page = self
            .items
            .get_playlist_items_with_access(playlist_id, user_id, LINKED_CHILD_MANUAL)
            .await?;
        let Some(access) = page.access else {
            // No member rows: the join can't tell "empty playlist" from
            // "missing/invisible playlist", so resolve access on its own. Only
            // empty playlists pay for this second query.
            self.access(playlist_id, user_id).await?;
            return Ok(Vec::new());
        };
        // Every row repeats the same access columns; the first decides.
        if decide_access(
            access.owner_user_id.as_deref(),
            access.open_access,
            access.share_can_edit,
            user_id,
        )
        .is_none()
        {
            return Err(ServiceError::not_found(format!("playlist {playlist_id}")));
        }
        // Every member (`GetManageableItems`): `GET /Playlists/{id}` lists
        // them all as `ItemIds`, and the items endpoint drops the ones the
        // caller may not see itself, as `PlaylistsController` does.
        self.require_visible(playlist_id, user_id).await?;
        Ok(page.items)
    }

    async fn add_user_to_shares(
        &self,
        request: &PlaylistUserUpdateRequest,
    ) -> Result<(), ServiceError> {
        // The share row, committed with its `Data` sync: each boot rebuilds
        // the shares from `Data`.
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        item_data::import_playlist_meta_if_missing_in(&mut tx, request.id).await?;
        item_data::share_playlist_in(
            &mut tx,
            request.id,
            request.user_id,
            request.can_edit.unwrap_or(false),
        )
        .await?;
        item_data::sync_container_data_in(&mut tx, request.id).await?;
        tx.commit().await.map_err(db_err)?;
        self.announce_updated(request.id).await;
        Ok(())
    }

    async fn remove_user_from_shares(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
        _share: &PlaylistUserPermissions,
    ) -> Result<(), ServiceError> {
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        item_data::import_playlist_meta_if_missing_in(&mut tx, playlist_id).await?;
        sqlx::query(
            r#"DELETE FROM "FerrofinPlaylistShares" WHERE "PlaylistId" = ?1 AND "UserId" = ?2"#,
        )
        .bind(guid_to_db(playlist_id))
        .bind(guid_to_db(user_id))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        item_data::sync_container_data_in(&mut tx, playlist_id).await?;
        tx.commit().await.map_err(db_err)?;
        self.announce_updated(playlist_id).await;
        Ok(())
    }

    async fn get_playlist_shares(
        &self,
        playlist_id: Uuid,
    ) -> Result<Vec<PlaylistUserPermissions>, ServiceError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT "UserId", "CanEdit" FROM "FerrofinPlaylistShares"
               WHERE "PlaylistId" = ?1 ORDER BY rowid"#,
        )
        .bind(guid_to_db(playlist_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|(uid, can_edit)| {
                Uuid::parse_str(&uid)
                    .ok()
                    .map(|user_id| PlaylistUserPermissions {
                        user_id,
                        can_edit: can_edit != 0,
                    })
            })
            .collect())
    }

    async fn add_item_to_playlist(
        &self,
        playlist_id: Uuid,
        item_ids: &[Uuid],
        position: Option<i32>,
        user_id: Uuid,
    ) -> Result<(), ServiceError> {
        // C# `PlaylistManager.AddToPlaylistInternal` (12.0): no duplicate
        // check — an item already in the playlist gets a second entry — and
        // the new block goes at `position` (≤ 0 prepends, ≥ length appends,
        // otherwise a splice), else at the end.
        self.require_playlist_argument(&playlist_id.to_string())
            .await?;
        let added: Vec<Entry> = self
            .playlist_items(item_ids, user_id)
            .await?
            .into_iter()
            .map(|id| (guid_to_db(id), i64::from(LINKED_CHILD_MANUAL)))
            .collect();
        // "Do nothing else if there are no items to add to the playlist."
        if added.is_empty() {
            return Ok(());
        }
        let edit = self.entry_edits.lock().await;
        let pid = guid_to_db(playlist_id);
        let mut entries = read_entries(self.db.pool(), &pid).await?;
        let at = position.map_or(entries.len(), |pos| {
            usize::try_from(pos.max(0))
                .unwrap_or(usize::MAX)
                .min(entries.len())
        });
        entries.splice(at..at, added);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        write_entries_in(&mut tx, &pid, &entries).await?;
        crate::item_persistence_service::stamp_playlist_in(
            &mut tx,
            playlist_id,
            None,
            Some(chrono::Utc::now()),
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        self.update_rating_to_items(playlist_id).await?;
        drop(edit);
        // "Refresh playlist metadata" (`AddToPlaylistInternal`).
        queue_refresh(self.providers.as_deref(), playlist_id, force_save_refresh()).await
    }

    async fn remove_item_from_playlist(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), ServiceError> {
        // An entry id is the child item id (see module docs). The wire form is
        // a lower-case `Guid('N')` (`GetPlaylistItems`' `ItemId?
        // .ToString("N")`), but `ChildId` is stored in the canonical dashed
        // uppercase form. Parse and re-format via `guid_to_db` to
        // normalise back to the stored form; skip any entry id that isn't a
        // valid GUID.
        // C# `RemoveItemFromPlaylistAsync` (12.0): every entry whose item id
        // is in the request goes — all occurrences, not the first.
        let playlist = self.require_playlist_argument(playlist_id).await?;
        let edit = self.entry_edits.lock().await;
        let pid = guid_to_db(playlist);
        let removals: Vec<String> = entry_ids
            .iter()
            .filter_map(|e| Uuid::parse_str(e).ok())
            .map(guid_to_db)
            .collect();
        let mut entries = read_entries(self.db.pool(), &pid).await?;
        let before = entries.len();
        entries.retain(|(child, _)| !removals.contains(child));
        if entries.len() != before {
            write_entries(self.db.writer(), &pid, &entries).await?;
        }
        self.update_rating_to_items(playlist).await?;
        drop(edit);
        // `RemoveItemFromPlaylistAsync` queues its refresh whatever it removed.
        queue_refresh(self.providers.as_deref(), playlist, force_save_refresh()).await
    }

    async fn move_item(
        &self,
        playlist_id: &str,
        entry_id: &str,
        new_index: i32,
        calling_user_id: Uuid,
    ) -> Result<(), ServiceError> {
        // Port of `Playlist.MoveItem`: pull the current order, relocate the entry,
        // then rewrite the `SortOrder` ordinals. `entry_id` is the `ChildId` (as
        // `remove_item_from_playlist` treats it).
        // Same ordering the read path uses (`get_linked_children_ids`), so the
        // relocation matches what the client sees.
        let playlist = self.require_playlist_argument(playlist_id).await?;
        let _edit = self.entry_edits.lock().await;
        let pid = guid_to_db(playlist);
        let mut entries = read_entries(self.db.pool(), &pid).await?;

        // `entry_id` arrives dashless (`Guid('N')`); `ChildId` is stored in the
        // canonical dashed uppercase form. Normalise before locating the entry
        // position (see `remove_item_from_playlist`). With duplicates legal,
        // the FIRST occurrence moves (C# `MoveItemAsync`: `FirstOrDefault`).
        let Ok(child_id) = Uuid::parse_str(entry_id) else {
            return Ok(()); // not a GUID — nothing to move
        };
        let needle = guid_to_db(child_id);
        let Some(pos) = entries.iter().position(|(c, _)| *c == needle) else {
            return Ok(()); // entry not in the playlist — nothing to move
        };
        // `new_index` counts the members the caller sees — the list
        // `GET /Playlists/{id}/Items` gave them (`MoveItemAsync`) — so the
        // entry lands right after the member it must follow in that list,
        // hidden members keeping their places. Upstream anchors on the list
        // before the entry is taken out (`DetermineAdjustedIndex`) and lands
        // one place short when moving down past a hidden member; this keeps
        // the index the caller asked for. With every member visible it is a
        // plain move to `new_index`.
        let mut visible = self.visible_entries(&entries, calling_user_id).await?;
        let new_index = usize::try_from(new_index.max(0)).unwrap_or(usize::MAX);
        if visible[pos] && visible[..pos].iter().filter(|v| **v).count() == new_index {
            return Ok(());
        }
        let entry = entries.remove(pos);
        visible.remove(pos);
        let shown: Vec<usize> = (0..entries.len()).filter(|i| visible[*i]).collect();
        let target = match new_index {
            0 => shown.first().copied().unwrap_or(0),
            n => shown.get(n - 1).map_or(entries.len(), |prior| prior + 1),
        };
        entries.insert(target, entry);
        write_entries(self.db.writer(), &pid, &entries).await?;
        self.announce_updated(playlist).await;
        Ok(())
    }

    async fn remove_playlists(&self, user_id: Uuid) -> Result<(), ServiceError> {
        // C# `RemovePlaylistsAsync`, over the playlists the user owns.
        // Upstream walks every playlist the user can see (`GetUserPlaylists`),
        // so it also hands away another user's playlist shared with or open
        // to them, and passes over one they own but cannot see past a
        // parental limit; and it can hand a playlist to the share of a user
        // who no longer exists (the deleted user's own, or one an earlier
        // deletion left behind), which leaves it owned by nobody. Both are
        // Jellyfin bugs not ported: only owned playlists are walked, and only
        // the shares of existing users inherit.
        // The user row is gone by now, so a failed playlist cannot be retried
        // through the API: one failure does not stop the others, and the
        // first is returned once all have been tried.
        let mut first_error = None;
        for playlist in item_data::playlists_owned_by(&self.db, user_id).await? {
            let outcome = async {
                // A hand-over commits with its `Data` sync, as every boot
                // re-imports the owner and the shares from `Data`.
                let mut tx = self.db.writer().begin().await.map_err(db_err)?;
                item_data::import_playlist_meta_if_missing_in(&mut tx, playlist).await?;
                let Some(permissions) =
                    item_data::playlist_permissions_in(&mut tx, playlist).await?
                else {
                    return Ok(());
                };
                if permissions.owner != Some(user_id) {
                    return Ok(()); // no longer theirs since the listing
                }
                // `Shares.OrderByDescending(x => x.CanEdit)` is a stable sort:
                // the first editing share in `Shares` order, else the first.
                let heir = permissions
                    .shares
                    .iter()
                    .find(|share| share.can_edit)
                    .or_else(|| permissions.shares.first())
                    .map(|share| share.user_id);
                if let Some(heir) = heir {
                    item_data::hand_playlist_to_in(&mut tx, playlist, heir).await?;
                }
                tx.commit().await.map_err(db_err)?;
                if heir.is_some() {
                    self.announce_updated(playlist).await;
                }
                if heir.is_none() && !permissions.open_access {
                    // `DeleteItem` with `DeleteFileLocation = false`.
                    let options = ferrofin_traits::options::DeleteOptions {
                        delete_file_location: false,
                        delete_from_external_provider: false,
                    };
                    self.library_manager.delete_item(playlist, &options).await?;
                }
                Ok::<(), ServiceError>(())
            }
            .await;
            match outcome {
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(error) => tracing::error!(
                    item_id = %playlist,
                    user_id = %user_id,
                    %error,
                    "could not hand over or delete a deleted user's playlist"
                ),
                Ok(()) => {}
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod playlist_announcements_tests;
#[cfg(test)]
mod remove_playlists_tests;

/// One playlist entry: `(ChildId, ChildType)` as stored.
type Entry = (String, i64);

/// A playlist's entries in stored order. Entries are positions, not a set:
/// the same item may appear more than once (12.0's `(ParentId, SortOrder)` key).
async fn read_entries(
    pool: &sqlx::SqlitePool,
    playlist_id: &str,
) -> Result<Vec<Entry>, ServiceError> {
    sqlx::query_as(
        r#"SELECT "ChildId", "ChildType" FROM "LinkedChildren"
           WHERE "ParentId" = ?1 ORDER BY "SortOrder""#,
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)
}

/// Replaces a playlist's entries wholesale with `entries`, `SortOrder`
/// dense from 0 in the given order, in one transaction — what 12.0's
/// `ItemPersistenceService` does on every folder save (delete the parent's
/// rows, re-insert the array). Shared by remove and move; a create, an add
/// and an update's replace run [`write_entries_in`] inside their own writer
/// transaction.
async fn write_entries(
    pool: &sqlx::SqlitePool,
    playlist_id: &str,
    entries: &[Entry],
) -> Result<(), ServiceError> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    write_entries_in(&mut tx, playlist_id, entries).await?;
    tx.commit().await.map_err(db_err)
}

/// [`write_entries`] on a connection the caller holds, normally a writer
/// transaction that commits more than the entries.
async fn write_entries_in(
    conn: &mut sqlx::SqliteConnection,
    playlist_id: &str,
    entries: &[Entry],
) -> Result<(), ServiceError> {
    sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1"#)
        .bind(playlist_id)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    for (ordinal, (child, child_type)) in entries.iter().enumerate() {
        sqlx::query(
            r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
               VALUES (?1, ?2, ?3, ?4)"#,
        )
        .bind(playlist_id)
        .bind(i64::try_from(ordinal).unwrap_or(i64::MAX))
        .bind(child)
        .bind(child_type)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::playlists::{PlaylistCreationRequest, PlaylistUpdateRequest};
    use uuid::Uuid;

    use ferrofin_traits::collections::{
        CollectionCreationOptions, CollectionManager, PlaylistManager,
    };

    use crate::linked_children_service::FerrofinLinkedChildrenService;
    use crate::test_support::{item_repository_over, library_manager_over, seed_item, test_db};

    use super::{
        COLLECTIONS_FOLDER_DATA, FerrofinCollectionManager, FerrofinPlaylistManager,
        collections_folder,
    };

    /// A playlist owned by the empty GUID — one the library scan found — is
    /// open to read for every caller and owned by none of them; even an empty
    /// caller, which the manager refuses before asking, would not own it.
    #[test]
    fn the_empty_owner_is_nobody() {
        use ferrofin_traits::collections::{PlaylistAccess, PlaylistAccessLevel};
        let nil = ferrofin_db::store::guid_to_db(Uuid::nil());
        for caller in [Uuid::nil(), Uuid::new_v4()] {
            assert_eq!(
                super::decide_access(Some(&nil), Some(1), None, caller),
                Some(PlaylistAccess {
                    level: PlaylistAccessLevel::Read,
                    open_access: true,
                }),
                "{caller}"
            );
            assert_eq!(
                super::decide_access(Some(&nil), Some(0), None, caller),
                None
            );
        }
    }

    /// Test paths under a temp root, so the provisioned containers land
    /// somewhere harmless.
    fn test_paths() -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
        test_paths_in(std::path::Path::new("/tmp/ferrofin-collections-test"))
    }

    /// Test paths under `root`: a created playlist makes a folder there.
    pub(super) fn test_paths_in(
        root: &std::path::Path,
    ) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
        Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
            root,
            root.join("log"),
            root.join("config"),
            root.join("cache"),
            root.join("web"),
        ))
    }

    fn collection_manager_over(db: &ferrofin_db::Database) -> FerrofinCollectionManager {
        FerrofinCollectionManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
            test_paths(),
        )
    }

    /// A created playlist has to stay reachable too, and must reuse the
    /// playlists folder the user-view manager already provisions.
    ///
    /// Ferrofin writes that folder as `ManualPlaylistsFolder` and an adopted
    /// database carries a `PlaylistsFolder` — at the same path. If the two
    /// provisioners disagreed there would be two rows, and a playlist parented
    /// to the one the query scope does not accept simply vanishes.
    /// A container provisioned before the user root existed is adopted by it
    /// later, rather than staying parentless forever because the row is already
    /// there.
    #[tokio::test]
    async fn a_parentless_container_is_attached_once_the_root_appears() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root_dir = tmp.path().to_string_lossy().into_owned();
        let paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
            Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
                root_dir.clone(),
                format!("{root_dir}/log"),
                format!("{root_dir}/config"),
                format!("{root_dir}/cache"),
                format!("{root_dir}/web"),
            ));
        let mode = crate::item_type_lookup::IdDerivation::from_meta(
            Some("jellyfin-10.11.8"),
            Some(paths.program_data_path()),
        );
        let root =
            crate::item_type_lookup::user_root_folder_id(&mode, &paths.default_user_views_path())
                .expect("root id");
        let path = format!("{}/collections", paths.data_path());

        // First provision: the root does not exist yet, so the row is created
        // without a parent rather than failing the foreign key.
        let id = crate::item_persistence_service::ensure_container(
            &db,
            BaseItemKind::CollectionFolder,
            "Collections",
            &path,
            &mode,
            Some(root),
            Some(COLLECTIONS_FOLDER_DATA),
        )
        .await
        .expect("provision")
        .expect("an id");
        assert!(
            crate::test_support::fetch_item(&db, id)
                .await
                .parent_id
                .is_none(),
            "nothing to parent to yet"
        );

        // The root appears, and the next provision adopts the existing row.
        crate::test_support::seed_named_item(
            &db,
            root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
        )
        .await;
        let again = crate::item_persistence_service::ensure_container(
            &db,
            BaseItemKind::CollectionFolder,
            "Collections",
            &path,
            &mode,
            Some(root),
            Some(COLLECTIONS_FOLDER_DATA),
        )
        .await
        .expect("provision")
        .expect("an id");
        assert_eq!(again, id, "the same row, not a second one");
        assert_eq!(
            crate::test_support::fetch_item(&db, id).await.parent_id,
            Some(ferrofin_db::store::guid_to_db(root)),
            "…now attached to the root"
        );
    }

    /// `get_collections_folder` is the real `EnsureLibraryFolder`: it answers
    /// `None` before the library exists when the caller did not ask for it to be
    /// created, and the provisioned row afterwards. It used to answer `None`
    /// unconditionally.
    #[tokio::test]
    async fn get_collections_folder_looks_up_and_provisions() {
        let db = test_db().await;
        let manager = collection_manager_over(&db);
        assert!(
            manager
                .get_collections_folder(false)
                .await
                .expect("lookup")
                .is_none(),
            "no library yet, and the caller did not ask for one"
        );
        let made = manager
            .get_collections_folder(true)
            .await
            .expect("provision")
            .expect("a row");
        assert_eq!(made.name.as_deref(), Some("Collections"));
        assert_eq!(made.data.as_deref(), Some(COLLECTIONS_FOLDER_DATA));
        let found = manager
            .get_collections_folder(false)
            .await
            .expect("lookup")
            .expect("a row");
        assert_eq!(found.id, made.id, "the same row, found by path");
    }

    /// gl-385: a new Collections library is named in the server UI culture,
    /// whatever the requesting client's language (v12.2
    /// `CollectionManager.EnsureLibraryFolder`, `GetServerLocalizedString`).
    #[tokio::test]
    async fn the_collections_library_takes_the_server_culture() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = FerrofinCollectionManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
            test_paths_in(tmp.path()),
        )
        .with_localization(Arc::new(
            crate::LocalizationManager::default().with_ui_culture("de"),
        ));
        let made = ferrofin_util::current_culture::scope_with_ui(
            "fr".into(),
            Some("fr".into()),
            manager.get_collections_folder(true),
        )
        .await
        .expect("provision")
        .expect("a row");
        assert_eq!(made.name.as_deref(), Some("Sammlungen"));
    }

    /// gl-922: the Collections library's id comes from its program-data-relative
    /// path, its stored `Path` from the absolute one, so a data directory that
    /// moved leaves the row at its old path under the same id. Both lookups
    /// find it there and move it to the new path: box-set grouping keeps
    /// finding its collections, and a create no longer fails on the primary key.
    #[tokio::test]
    async fn a_moved_data_directory_keeps_its_collections_library() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager_in = |dir: &str| {
            FerrofinCollectionManager::new(
                db.clone(),
                library_manager_over(db.clone()),
                Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
                test_paths_in(&tmp.path().join(dir)),
            )
        };
        let made = manager_in("old")
            .get_collections_folder(true)
            .await
            .expect("provision")
            .expect("a row");

        let moved = manager_in("new");
        let found = moved
            .get_collections_folder(false)
            .await
            .expect("lookup")
            .expect("the row under its derived id");
        assert_eq!(found.id, made.id);
        let new_path = format!("{}/new/data/collections", tmp.path().display());
        assert_eq!(found.path.as_deref(), Some(new_path.as_str()));

        moved
            .create_collection(&CollectionCreationOptions {
                name: "Heist".to_owned(),
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("a create after the move");
    }

    /// gl-922: only a `CollectionFolder` under the derived id moves. A row of
    /// another kind there, or an adopted Collections library under its own
    /// id, keeps its path.
    #[tokio::test]
    async fn only_the_derived_collections_row_moves() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths_in(tmp.path());
        let path = format!("{}/collections", paths.data_path());
        let mode =
            crate::item_type_lookup::IdDerivation::from_meta(None, Some(paths.program_data_path()));
        let derived = crate::item_type_lookup::derive_item_id_with(
            &mode,
            BaseItemKind::CollectionFolder,
            &path,
        )
        .expect("an id");
        let adopted = Uuid::new_v4();
        for (id, kind, old) in [
            (derived, BaseItemKind::Folder, "/old/data/collections"),
            (
                adopted,
                BaseItemKind::CollectionFolder,
                "/old/root/default/Collections",
            ),
        ] {
            crate::test_support::seed_named_item(&db, id, kind, "Collections").await;
            let mut row = crate::test_support::fetch_item(&db, id).await;
            row.path = Some(old.to_owned());
            crate::test_support::save_item(&db, &row).await;
        }

        let found = crate::item_persistence_service::moved_container(
            &db,
            BaseItemKind::CollectionFolder,
            &path,
            &mode,
        )
        .await
        .expect("lookup");
        assert_eq!(found, None);
        for (id, old) in [
            (derived, "/old/data/collections"),
            (adopted, "/old/root/default/Collections"),
        ] {
            let row = crate::test_support::fetch_item(&db, id).await;
            assert_eq!(row.path.as_deref(), Some(old));
        }
    }

    /// The auto-provisioned Collections library carries
    /// `"CollectionType":"boxsets"` in `Data` — the column 10.11.8 reads that
    /// field from.
    ///
    /// Without it, `/Items?parentId={root}`, `/UserViews` and
    /// `/Library/MediaFolders` all sent a null `CollectionType` for the folder
    /// where Jellyfin sends `boxsets` (measured on the parity pair 2026-08-31).
    #[tokio::test]
    async fn the_collections_library_carries_the_boxsets_collection_type() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root_dir = tmp.path().to_string_lossy().into_owned();
        let paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
            Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
                root_dir.clone(),
                format!("{root_dir}/log"),
                format!("{root_dir}/config"),
                format!("{root_dir}/cache"),
                format!("{root_dir}/web"),
            ));
        let id = collections_folder(&db, &paths, "Collections")
            .await
            .expect("provision")
            .expect("an id");
        let data = crate::test_support::fetch_item(&db, id).await.data;
        assert_eq!(
            data.as_deref(),
            Some(COLLECTIONS_FOLDER_DATA),
            "the provisioned row carries the boxsets collection type"
        );
        // …and the DTO layer really reads it back as `boxsets`, which is the
        // observable the parity pair measures.
        let parsed: serde_json::Value =
            serde_json::from_str(data.as_deref().expect("data")).expect("valid json");
        assert_eq!(
            parsed.get("CollectionType").and_then(|v| v.as_str()),
            Some("boxsets")
        );

        // The other half — that an ADOPTED database's richer blob survives
        // re-provisioning — lives in `item_persistence_service`'s own tests,
        // because seeding it is SQL and SQL stays behind that boundary.
    }

    /// The provisioned container carries the id JELLYFIN would compute for that
    /// directory, its directory exists, and it hangs off the user root.
    ///
    /// The id is the one that matters for drop-in: `ensure_container` used to
    /// derive it with `IdDerivation::LegacyLowercase` whatever the database
    /// said, so on an adopted (Jellyfin-parity) database Jellyfin would look for
    /// its own id, miss, and create a SECOND Collections library beside ours —
    /// with the round-trip guarantee going with it.
    #[tokio::test]
    async fn a_provisioned_container_is_shaped_the_way_jellyfin_expects() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root_dir = tmp.path().to_string_lossy().into_owned();
        let paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
            Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
                root_dir.clone(),
                format!("{root_dir}/log"),
                format!("{root_dir}/config"),
                format!("{root_dir}/cache"),
                format!("{root_dir}/web"),
            ));

        // A parity database, as a fresh or adopted one is.
        db.meta_set("item_id_derivation", "jellyfin-10.11.8")
            .await
            .expect("mode");
        // The user root has to exist for the container to be parented to it.
        let mode = crate::item_type_lookup::IdDerivation::from_meta(
            Some("jellyfin-10.11.8"),
            Some(paths.program_data_path()),
        );
        let root =
            crate::item_type_lookup::user_root_folder_id(&mode, &paths.default_user_views_path())
                .expect("root id");
        crate::test_support::seed_named_item(
            &db,
            root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
        )
        .await;

        let path = format!("{}/collections", paths.data_path());
        let id = crate::item_persistence_service::ensure_container(
            &db,
            BaseItemKind::CollectionFolder,
            "Collections",
            &path,
            &mode,
            Some(root),
            Some(COLLECTIONS_FOLDER_DATA),
        )
        .await
        .expect("provision")
        .expect("an id");

        // The id Jellyfin derives for that directory, under the same rule.
        let expected = crate::item_type_lookup::derive_item_id_with(
            &mode,
            BaseItemKind::CollectionFolder,
            &path,
        )
        .expect("expected id");
        assert_eq!(id, expected, "the container carries Jellyfin's id");
        assert_ne!(
            id,
            crate::item_type_lookup::derive_item_id(BaseItemKind::CollectionFolder, &path)
                .expect("legacy id"),
            "…which is NOT the legacy-lowercase one, or this test proves nothing"
        );

        assert!(
            tokio::fs::metadata(&path).await.is_ok(),
            "the directory the row describes exists"
        );
        let row = crate::test_support::fetch_item(&db, id).await;
        assert_eq!(
            row.parent_id.as_deref(),
            Some(ferrofin_db::store::guid_to_db(root).as_str()),
            "and it hangs off the user root"
        );
    }

    #[tokio::test]
    async fn a_created_playlist_reuses_the_existing_playlists_folder() {
        let db = test_db().await;
        let (mgr, root) = bare_playlist_manager(&db);
        let folder = root
            .path()
            .join("data/playlists")
            .to_string_lossy()
            .into_owned();
        // A folder already at the playlists path under an id of its own: the
        // `ManualPlaylistsFolder` row an earlier Ferrofin wrote there.
        let existing = Uuid::from_u128(0xF0DE);
        crate::item_persistence_service::insert_named_item(
            &db,
            existing,
            BaseItemKind::ManualPlaylistsFolder,
            "Playlists",
            true,
            None,
            false,
        )
        .await
        .expect("seed the existing folder");
        crate::test_support::set_item_path(&db, existing, &folder).await;
        // …and the start that finds it there.
        provision_root_folder(&db, &mgr.paths).await;

        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("My Playlist".to_owned()),
                user_id: Uuid::new_v4(),
                ..Default::default()
            })
            .await
            .expect("create playlist");

        let row = crate::test_support::fetch_item(
            &db,
            Uuid::parse_str(&created.id).expect("playlist id"),
        )
        .await;
        assert_eq!(
            row.parent_id,
            Some(ferrofin_db::store::guid_to_db(existing)),
            "the playlist reuses the folder already there, rather than a second one"
        );

        let folders = crate::test_support::items_at_path(&db, &folder).await;
        assert_eq!(folders, 1, "exactly one playlists folder at that path");
    }

    /// gl-910: `PlaylistManager.CreatePlaylist` only looks the playlists
    /// folder up (`GetPlaylistsFolder`, PlaylistManager.cs:566-572) and
    /// throws `ArgumentException(nameof(parentFolder))` when there is none
    /// (:84-88), a 400. It never makes the folder — startup does — so a
    /// create without one writes nothing.
    #[tokio::test]
    async fn a_create_without_a_playlists_folder_is_refused_and_writes_nothing() {
        let db = test_db().await;
        let (mgr, root) = bare_playlist_manager(&db);

        let result = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Nowhere".to_owned()),
                user_id: Uuid::new_v4(),
                ..Default::default()
            })
            .await;

        assert!(
            matches!(
                &result,
                Err(ferrofin_traits::error::ServiceError::InvalidInput(why)) if why == "parentFolder"
            ),
            "{result:?}"
        );
        let rows = item_repository_over(db.clone())
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![
                    BaseItemKind::Playlist,
                    BaseItemKind::PlaylistsFolder,
                    BaseItemKind::ManualPlaylistsFolder,
                ],
                ..Default::default()
            })
            .await
            .expect("rows");
        assert!(rows.is_empty(), "{rows:?}");
        assert!(!root.path().join("data/playlists").exists());
    }

    /// gl-910: after the data directory moved, the playlists folder's row
    /// keeps its id (derived from the program-data-relative `data\playlists`)
    /// but not its path. A create finds the folder by that id, as upstream's
    /// lookup finds it whatever its path, where it used to insert the same id
    /// a second time and fail; and the next start saves the folder at its
    /// new path, which creates go on using.
    #[tokio::test]
    async fn a_create_finds_a_playlists_folder_left_at_a_moved_data_directory() {
        let db = test_db().await;
        let (mgr, root) = playlist_manager(&db).await;
        let (mode, _) = super::container_identity(&db, &mgr.paths)
            .await
            .expect("mode");
        let current = root.path().join("data/playlists");
        let folder = crate::item_type_lookup::derive_item_id_with(
            &mode,
            BaseItemKind::PlaylistsFolder,
            &current.to_string_lossy(),
        )
        .expect("folder id");
        crate::test_support::set_item_path(&db, folder, "/moved-from/data/playlists").await;
        let request = |name: &str| PlaylistCreationRequest {
            name: Some(name.to_owned()),
            user_id: Uuid::new_v4(),
            ..Default::default()
        };

        let before = mgr
            .create_playlist(&request("Before the restart"))
            .await
            .expect("create before the restart");
        provision_root_folder(&db, &mgr.paths).await;
        let after = mgr
            .create_playlist(&request("After the restart"))
            .await
            .expect("create after the restart");

        for created in [before, after] {
            let row = crate::test_support::fetch_item(
                &db,
                Uuid::parse_str(&created.id).expect("playlist id"),
            )
            .await;
            assert_eq!(row.parent_id, Some(ferrofin_db::store::guid_to_db(folder)));
        }
        assert_eq!(
            crate::test_support::fetch_item(&db, folder).await.path,
            Some(current.to_string_lossy().into_owned()),
            "the start re-paths the folder"
        );
    }

    /// gl-597: a created playlist is stored as `PlaylistManager.CreatePlaylist`
    /// stores one: in a folder of its own in the playlists folder, named
    /// `GetValidFilename(name)` with `1` appended while that is taken
    /// (`GetTargetPath`), under the id `Folder.AddChild` derives from that
    /// path. Without the folder an upload had nowhere to be saved beside, and
    /// without the id Jellyfin's rescan would delete the row. The ids are
    /// Jellyfin's own, worked out apart from Ferrofin: the MD5 of the UTF-16LE
    /// `MediaBrowser.Controller.Playlists.Playlist` + `data\playlists\<folder>`
    /// (`GetNewItemIdInternal`, case-sensitive ids by default).
    #[tokio::test]
    async fn a_created_playlist_gets_a_folder_and_the_id_its_path_derives() {
        let db = test_db().await;
        db.meta_set("item_id_derivation", "jellyfin-10.11.8")
            .await
            .expect("derivation");
        let (mgr, root) = playlist_manager(&db).await;
        let playlists = root.path().join("data/playlists");
        for (folder, jellyfin_id) in [
            ("Road  Trip ", "ac169649-cd0e-fce1-b4c2-0f38aa100497"),
            ("Road  Trip 1", "2431360a-7a6f-47bc-9bb7-60c9e32fa505"),
        ] {
            let created = mgr
                .create_playlist(&PlaylistCreationRequest {
                    name: Some("Road: Trip?".to_owned()),
                    user_id: Uuid::new_v4(),
                    ..Default::default()
                })
                .await
                .expect("create playlist");
            let path = playlists.join(folder);
            assert!(path.is_dir(), "{} is made", path.display());
            let path = path.to_string_lossy().into_owned();
            let id = Uuid::parse_str(&created.id).expect("playlist id");
            assert_eq!(
                crate::test_support::fetch_item(&db, id).await.path,
                Some(path.clone())
            );
            assert_eq!(id.to_string(), jellyfin_id);
        }
    }

    /// A playlist already at the id a new folder derives (one whose folder
    /// went missing) gets that folder back, and the create takes the next
    /// name rather than failing on the stored id.
    #[tokio::test]
    async fn a_created_playlist_skips_a_name_whose_id_is_taken() {
        let db = test_db().await;
        let (mgr, root) = playlist_manager(&db).await;
        let (mode, _) = super::container_identity(&db, &mgr.paths)
            .await
            .expect("mode");
        let playlists = root.path().join("data/playlists");
        let taken_path = playlists.join("Mix").to_string_lossy().into_owned();
        let taken = crate::item_type_lookup::derive_item_id_with(
            &mode,
            BaseItemKind::Playlist,
            &taken_path,
        )
        .expect("id");
        crate::test_support::seed_named_item(&db, taken, BaseItemKind::Playlist, "Mix").await;
        crate::test_support::set_item_path(&db, taken, &taken_path).await;

        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Mix".to_owned()),
                user_id: Uuid::new_v4(),
                ..Default::default()
            })
            .await
            .expect("create playlist");
        let id = Uuid::parse_str(&created.id).expect("playlist id");
        assert_ne!(id, taken);
        let next = playlists.join("Mix1").to_string_lossy().into_owned();
        assert_eq!(
            crate::test_support::fetch_item(&db, id).await.path,
            Some(next)
        );
        assert!(std::path::Path::new(&taken_path).is_dir());
    }

    /// An empty name gets a folder inside the playlists folder, never the
    /// `playlists1` beside it that upstream's `Path.Combine` makes of it.
    #[tokio::test]
    async fn a_playlist_with_no_name_gets_a_folder_inside_the_playlists_folder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let playlists = tmp.path().join("playlists").to_string_lossy().into_owned();
        let path = super::new_playlist_folder(&playlists, "")
            .await
            .expect("folder");
        assert_eq!(path, format!("{playlists}/1"));
        assert!(std::path::Path::new(&path).is_dir());
    }

    /// A created collection has to stay reachable.
    ///
    /// A query naming no scope is confined to the user's libraries, so an item
    /// with no parent and no top parent is invisible to every user browse.
    /// Upstream never creates one: `CreateCollectionAsync` goes through
    /// `EnsureLibraryFolder`, which auto-provisions the "Collections" library.
    #[tokio::test]
    async fn a_created_collection_lands_in_the_collections_library() {
        let db = test_db().await;
        let mgr = collection_manager_over(&db);
        let row = mgr
            .create_collection(&CollectionCreationOptions {
                name: "My Collection".to_owned(),
                ..Default::default()
            })
            .await
            .expect("create");

        let parent = row.parent_id.expect("a collection has a parent");
        assert_eq!(
            row.top_parent_id.as_deref(),
            Some(parent.as_str()),
            "the library is also the top parent"
        );
        let container = crate::test_support::fetch_item(
            &db,
            Uuid::parse_str(&parent).expect("the parent id is a guid"),
        )
        .await;
        assert_eq!(container.name.as_deref(), Some("Collections"));
        assert_eq!(
            container.type_,
            crate::item_type_lookup::stored_type_name(BaseItemKind::CollectionFolder)
                .expect("kind is known")
        );
    }

    /// C# `CreateCollectionAsync` gives a new box set its own folder,
    /// `{data}/collections/<GetValidFilename(name)> [boxset]`, stores it as
    /// its `Path`, and `Folder.AddChild` derives its id from that path. A
    /// second box set of the same name takes the next free folder instead of
    /// the first one's id.
    #[tokio::test]
    async fn a_created_collection_gets_its_boxset_folder_and_path_derived_id() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths_in(tmp.path());
        let mgr = FerrofinCollectionManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
            Arc::clone(&paths),
        );
        let create = |name: &'static str| {
            let mgr = mgr.clone();
            async move {
                mgr.create_collection(&CollectionCreationOptions {
                    name: name.to_owned(),
                    ..Default::default()
                })
                .await
                .expect("create")
            }
        };
        let (mode, _) = super::container_identity(&db, &paths).await.expect("mode");
        let collections = format!("{}/collections", paths.data_path());

        // `GetValidFilename` turns the `:` into a space, trimming nothing.
        let first = create("My: Collection").await;
        let expected = format!("{collections}/My  Collection [boxset]");
        assert_eq!(first.path.as_deref(), Some(expected.as_str()));
        assert!(std::path::Path::new(&expected).is_dir());
        assert_eq!(
            Uuid::parse_str(&first.id).ok(),
            crate::item_type_lookup::derive_item_id_with(&mode, BaseItemKind::BoxSet, &expected)
        );

        let second = create("My: Collection").await;
        assert_ne!(second.id, first.id);
        assert_eq!(
            second.path.as_deref(),
            Some(format!("{collections}/My  Collection1 [boxset]").as_str())
        );
    }

    /// The container is provisioned once, and an existing one is reused — an
    /// adopted database keeps Jellyfin's rather than gaining a second.
    #[tokio::test]
    async fn the_collections_library_is_provisioned_once() {
        let db = test_db().await;
        let mgr = collection_manager_over(&db);
        let mut parents = Vec::new();
        for name in ["First", "Second"] {
            parents.push(
                mgr.create_collection(&CollectionCreationOptions {
                    name: name.to_owned(),
                    ..Default::default()
                })
                .await
                .expect("create")
                .parent_id
                .expect("parent"),
            );
        }
        assert_eq!(parents[0], parents[1]);
        let containers = crate::test_support::item_repository_over(db.clone())
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![BaseItemKind::CollectionFolder],
                ..Default::default()
            })
            .await
            .expect("libraries");
        assert_eq!(containers.len(), 1, "provisioned once, then reused");
    }

    /// The orphans an older Ferrofin created are adopted on the next create.
    #[tokio::test]
    async fn an_orphaned_collection_is_taken_into_the_library() {
        let db = test_db().await;
        let mgr = collection_manager_over(&db);
        // What the previous code wrote: no parent, no top parent.
        let orphan = Uuid::from_u128(0xB0C5);
        crate::item_persistence_service::insert_named_item(
            &db,
            orphan,
            BaseItemKind::BoxSet,
            "Old Collection",
            true,
            None,
            false,
        )
        .await
        .expect("orphan");

        mgr.create_collection(&CollectionCreationOptions {
            name: "New Collection".to_owned(),
            ..Default::default()
        })
        .await
        .expect("create");

        let adopted = crate::test_support::fetch_item(&db, orphan).await;
        assert!(
            adopted.parent_id.is_some(),
            "the orphan was taken into the library"
        );
        assert_eq!(adopted.parent_id, adopted.top_parent_id);
    }

    /// `CollectionCreationOptions.is_locked` reaches the stored row, as
    /// upstream's `CreateCollectionAsync` sets `IsLocked = options.IsLocked`.
    #[tokio::test]
    async fn create_collection_honours_is_locked() {
        let db = test_db().await;
        let mgr = collection_manager_over(&db);

        let locked = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Locked".to_owned(),
                is_locked: true,
                ..Default::default()
            })
            .await
            .expect("create locked");
        assert!(locked.is_locked, "the returned row carries the flag");
        let locked_id = Uuid::parse_str(&locked.id).expect("uuid");
        assert!(
            crate::test_support::fetch_item(&db, locked_id)
                .await
                .is_locked,
            "a box set created with isLocked=true is stored IsLocked = 1"
        );

        let unlocked = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Unlocked".to_owned(),
                ..Default::default()
            })
            .await
            .expect("create unlocked");
        let unlocked_id = Uuid::parse_str(&unlocked.id).expect("uuid");
        assert!(
            !crate::test_support::fetch_item(&db, unlocked_id)
                .await
                .is_locked,
            "the flag defaults to false"
        );
    }

    #[tokio::test]
    async fn create_collection_persists_row_and_members() {
        let db = test_db().await;
        let movie_a = Uuid::new_v4();
        let movie_b = Uuid::new_v4();
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        let data_dir = tempfile::tempdir().expect("tempdir");
        let items = item_repository_over(db.clone());
        let library = Arc::new(
            crate::library_manager::FerrofinLibraryManager::new(
                items.clone(),
                Arc::new(crate::item_count_service::FerrofinItemCountService::new(
                    db.clone(),
                )),
                Arc::new(
                    crate::item_persistence_service::FerrofinItemPersistenceService::new(
                        db.clone(),
                    ),
                ),
                Arc::new(crate::people_repository::FerrofinPeopleRepository::new(
                    db.clone(),
                )),
            )
            .with_visibility(Arc::new(crate::item_visibility::ItemVisibility::new(
                db.clone(),
                items,
                Arc::new(crate::LocalizationManager::new("US")),
                data_dir.path().to_string_lossy().into_owned(),
            ))),
        );
        let mgr = FerrofinCollectionManager::new(
            db.clone(),
            library,
            Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
            test_paths(),
        );
        let user = crate::test_support::seed_user_with_defaults(&db, Uuid::new_v4()).await;

        let options = CollectionCreationOptions {
            name: "Favourites".to_owned(),
            item_id_list: vec![movie_a],
            ..CollectionCreationOptions::default()
        };
        let created = mgr.create_collection(&options).await.expect("create");
        assert_eq!(created.name.as_deref(), Some("Favourites"));
        let collection_id = Uuid::parse_str(&created.id).expect("uuid");
        assert_eq!(
            crate::test_support::fetch_item(&db, collection_id)
                .await
                .clean_name
                .as_deref(),
            Some("favourites"),
            "a new box set is found by name"
        );

        mgr.add_to_collection(collection_id, &[movie_b])
            .await
            .expect("add");

        let containing = mgr
            .get_collections_containing_item(&user, movie_b)
            .await
            .expect("containing");
        assert_eq!(containing.len(), 1);
        assert_eq!(containing[0].id, created.id);
        assert!(
            mgr.get_collections_containing_item(&user, Uuid::nil())
                .await
                .expect("nil")
                .is_empty()
        );

        // C# resolves each set through `GetItemById<BoxSet>(id, user)`: a set
        // above the user's parental limit drops out.
        let mut rated = crate::test_support::fetch_item(&db, collection_id).await;
        rated.official_rating = Some("R".to_owned());
        crate::test_support::save_item(&db, &rated).await;
        let mut limited = user.clone();
        limited.max_parental_rating_score = Some(12);
        assert!(
            mgr.get_collections_containing_item(&limited, movie_b)
                .await
                .expect("limited")
                .is_empty(),
            "a set the user may not see is not listed"
        );

        mgr.remove_from_collection(collection_id, &[movie_b])
            .await
            .expect("remove");
        let containing = mgr
            .get_collections_containing_item(&user, movie_b)
            .await
            .expect("containing2");
        assert!(containing.is_empty());
    }

    /// Issue #55: a box set or playlist takes the least restrictive rating of
    /// its members whenever they change (`UpdateRatingToItems`), and the save
    /// re-derives its parental score. Unrated members are ignored and removing
    /// the only rated member keeps the rating. A locked box set still re-rates
    /// on a direct add (upstream's `AddToCollectionAsync`), but not on an add
    /// of nothing new or the refresh after a removal; a locked playlist keeps
    /// its rating; a locked `OfficialRating` keeps it everywhere.
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one set of members through every box-set and playlist membership path"
    )]
    async fn a_box_set_and_a_playlist_take_their_members_rating() {
        let db = test_db().await;
        let (heat, up, unrated) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for (id, rating) in [(heat, Some("R")), (up, Some("PG")), (unrated, None)] {
            seed_item(&db, id, BaseItemKind::Movie).await;
            let mut row = crate::test_support::fetch_item(&db, id).await;
            row.official_rating = rating.map(str::to_owned);
            crate::test_support::save_item(&db, &row).await;
        }
        let data_dir = tempfile::tempdir().expect("tempdir");
        let items = item_repository_over(db.clone());
        let visibility = Arc::new(crate::item_visibility::ItemVisibility::new(
            db.clone(),
            items.clone(),
            Arc::new(crate::LocalizationManager::new("US")),
            data_dir.path().to_string_lossy().into_owned(),
        ));
        let library: Arc<dyn ferrofin_traits::library::LibraryManager> = Arc::new(
            crate::library_manager::FerrofinLibraryManager::new(
                items.clone(),
                Arc::new(crate::item_count_service::FerrofinItemCountService::new(
                    db.clone(),
                )),
                Arc::new(
                    crate::item_persistence_service::FerrofinItemPersistenceService::new(
                        db.clone(),
                    ),
                ),
                Arc::new(crate::people_repository::FerrofinPeopleRepository::new(
                    db.clone(),
                )),
            )
            .with_visibility(Arc::clone(&visibility)),
        );
        let rating = |id| {
            let db = db.clone();
            async move {
                let row = crate::test_support::fetch_item(&db, id).await;
                (row.official_rating, row.inherited_parental_rating_value)
            }
        };
        let set_locked = |id, locked| {
            let db = db.clone();
            async move {
                let mut row = crate::test_support::fetch_item(&db, id).await;
                row.is_locked = locked;
                crate::test_support::save_item(&db, &row).await;
            }
        };
        let set_rating = |id, rating: &'static str| {
            let db = db.clone();
            async move {
                let mut row = crate::test_support::fetch_item(&db, id).await;
                row.official_rating = Some(rating.to_owned());
                crate::test_support::save_item(&db, &row).await;
            }
        };
        let collections = FerrofinCollectionManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
            test_paths(),
        )
        .with_visibility(Arc::clone(&visibility));

        let created = collections
            .create_collection(&CollectionCreationOptions {
                name: "Heat Collection".to_owned(),
                item_id_list: vec![heat, unrated],
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create");
        assert_eq!(created.official_rating.as_deref(), Some("R"));
        let set = Uuid::parse_str(&created.id).expect("uuid");
        assert_eq!(rating(set).await, (Some("R".to_owned()), Some(17)));
        collections
            .add_to_collection(set, &[up])
            .await
            .expect("add");
        assert_eq!(rating(set).await, (Some("PG".to_owned()), Some(10)));
        collections
            .remove_from_collection(set, &[up, heat])
            .await
            .expect("remove");
        assert_eq!(
            rating(set).await,
            (Some("PG".to_owned()), Some(10)),
            "no rated member left: the set keeps its rating"
        );
        // jellyfin-web creates box sets locked; a direct add still re-rates.
        set_locked(set, true).await;
        collections
            .add_to_collection(set, &[heat])
            .await
            .expect("re-add");
        assert_eq!(rating(set).await.0.as_deref(), Some("R"));
        set_rating(heat, "G").await;
        collections
            .add_to_collection(set, &[heat])
            .await
            .expect("add an existing member");
        assert_eq!(
            rating(set).await.0.as_deref(),
            Some("R"),
            "nothing new was added: no re-rate"
        );
        collections
            .remove_from_collection(set, &[])
            .await
            .expect("remove nothing");
        assert_eq!(
            rating(set).await.0.as_deref(),
            Some("R"),
            "the refresh after a removal leaves a locked set alone"
        );
        set_locked(set, false).await;
        library
            .update_item_locked_fields(
                set,
                &[ferrofin_db::enums::metadata_field::to_i32(
                    ferrofin_model::entities::MetadataField::OfficialRating,
                )],
            )
            .await
            .expect("lock the rating");
        collections
            .add_to_collection(set, &[up])
            .await
            .expect("add");
        assert_eq!(
            rating(set).await.0.as_deref(),
            Some("R"),
            "a locked rating stays"
        );

        set_rating(heat, "R").await;
        // A movie addressed as a collection is refused and keeps its own rating.
        assert!(matches!(
            collections.add_to_collection(heat, &[up]).await,
            Err(ferrofin_traits::error::ServiceError::InvalidInput(_))
        ));
        assert_eq!(rating(heat).await, (Some("R".to_owned()), None));
        let (playlists, _root) = playlist_manager(&db).await;
        let playlists = playlists.with_visibility(
            Arc::clone(&visibility),
            Arc::new(crate::user_manager::FerrofinUserManager::new(db.clone())),
        );
        let playlists = FerrofinPlaylistManager {
            library_manager: Arc::clone(&library),
            ..playlists
        };
        let created = playlists
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Mix".to_owned()),
                item_id_list: vec![unrated, heat],
                user_id: Uuid::new_v4(),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create playlist");
        let playlist = Uuid::parse_str(&created.id).expect("uuid");
        assert_eq!(rating(playlist).await, (Some("R".to_owned()), Some(17)));
        playlists
            .add_item_to_playlist(playlist, &[up], None, Uuid::nil())
            .await
            .expect("add to playlist");
        assert_eq!(rating(playlist).await.0.as_deref(), Some("PG"));
        playlists
            .update_playlist(&PlaylistUpdateRequest {
                id: playlist,
                ids: Some(vec![heat]),
                ..PlaylistUpdateRequest::default()
            })
            .await
            .expect("replace entries");
        assert_eq!(rating(playlist).await.0.as_deref(), Some("R"));
        set_locked(playlist, true).await;
        playlists
            .add_item_to_playlist(playlist, &[up], None, Uuid::nil())
            .await
            .expect("add to a locked playlist");
        playlists
            .remove_item_from_playlist(&playlist.to_string(), &[heat.simple().to_string()])
            .await
            .expect("remove from playlist");
        assert_eq!(
            rating(playlist).await.0.as_deref(),
            Some("R"),
            "a locked playlist keeps its rating"
        );
    }

    #[tokio::test]
    async fn collection_members_visible_via_parentid_browse() {
        // Live-path repro (WI-6): create a collection with one member, add a second,
        // then browse it the way GET /Items?parentId=<collection> does —
        // library.query_items -> item_repository.get_items -> translate_query.
        // Both members must surface (C# Folder.GetChildren merges LinkedChildren).
        use ferrofin_traits::options::InternalItemsQuery;
        let db = test_db().await;
        let movie_a = Uuid::new_v4();
        let movie_b = Uuid::new_v4();
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        let library = library_manager_over(db.clone());
        let mgr = collection_manager_over(&db);
        let created = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Watchlist".to_owned(),
                item_id_list: vec![movie_a],
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create");
        let cid = Uuid::parse_str(&created.id).expect("uuid");
        mgr.add_to_collection(cid, &[movie_b]).await.expect("add");

        let result = library
            .query_items(&InternalItemsQuery {
                parent_id: cid,
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse");
        let ids: Vec<Uuid> = result
            .items
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert!(ids.contains(&movie_a), "movie_a missing: {ids:?}");
        assert!(ids.contains(&movie_b), "movie_b missing: {ids:?}");
        assert_eq!(result.total_record_count, 2);
    }

    #[tokio::test]
    async fn deleting_a_collection_does_not_delete_its_members() {
        // Data-loss regression: DELETE /Items/{boxset} cascades to PHYSICAL children only.
        // A box-set/playlist's members are LinkedChildren (references); deleting the container
        // must leave the referenced items intact. (WI-6 made parentId browse merge LinkedChildren;
        // without physical_children_only the delete-cascade would wipe the movies.)
        use ferrofin_traits::options::DeleteOptions;
        let db = test_db().await;
        let movie_a = Uuid::new_v4();
        let movie_b = Uuid::new_v4();
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        let library = library_manager_over(db.clone());
        let mgr = collection_manager_over(&db);
        let created = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Doomed".to_owned(),
                item_id_list: vec![movie_a, movie_b],
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create");
        let cid = Uuid::parse_str(&created.id).expect("uuid");

        library
            .delete_item(cid, &DeleteOptions::default())
            .await
            .expect("delete collection");

        // The collection is gone, but both movies survive.
        assert!(library.get_item_by_id(cid).await.expect("q").is_none());
        assert!(
            library.get_item_by_id(movie_a).await.expect("q").is_some(),
            "movie_a deleted!"
        );
        assert!(
            library.get_item_by_id(movie_b).await.expect("q").is_some(),
            "movie_b deleted!"
        );
    }

    #[tokio::test]
    async fn create_and_update_playlist() {
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;

        let (mgr, _root) = playlist_manager(&db).await;

        // Reads below must come from the owner: playlists are visible only to
        // their owner / shared users now.
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Roadtrip".to_owned()),
                item_id_list: vec![track],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let row = mgr
            .get_playlist_for_user(playlist_id, owner)
            .await
            .expect("get");
        assert_eq!(row.name.as_deref(), Some("Roadtrip"));

        mgr.update_playlist(&PlaylistUpdateRequest {
            id: playlist_id,
            name: Some("Roadtrip 2".to_owned()),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("update");
        let row = mgr
            .get_playlist_for_user(playlist_id, owner)
            .await
            .expect("get2");
        assert_eq!(row.name.as_deref(), Some("Roadtrip 2"));

        let all = mgr.get_playlists(owner).await.expect("list");
        assert_eq!(all.len(), 1);

        // Removing the sole member leaves the playlist row intact.
        mgr.remove_item_from_playlist(&created.id, &[track.to_string()])
            .await
            .expect("remove");
        assert_eq!(mgr.get_playlists(owner).await.expect("list2").len(), 1);
    }

    #[tokio::test]
    async fn get_playlist_items_returns_members() {
        let db = test_db().await;
        let track_a = Uuid::new_v4();
        let track_b = Uuid::new_v4();
        seed_item(&db, track_a, BaseItemKind::Audio).await;
        seed_item(&db, track_b, BaseItemKind::Audio).await;

        let (mgr, _root) = playlist_manager(&db).await;

        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Mix".to_owned()),
                item_id_list: vec![track_a, track_b],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let items = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items");
        assert_eq!(items.len(), 2);
        let ids: Vec<Uuid> = items
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert!(ids.contains(&track_a));
        assert!(ids.contains(&track_b));
    }

    #[tokio::test]
    async fn get_playlist_items_preserves_link_order() {
        // Members must come back in link order, not id/insertion order — seed
        // three tracks and add them out of sort order; the result must echo the
        // added sequence. (No prior coverage guarded playlist ordering.)
        let db = test_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        for id in [a, b, c] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let (mgr, _root) = playlist_manager(&db).await;
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Ordered".to_owned()),
                item_id_list: vec![c, a, b],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let ids: Vec<Uuid> = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert_eq!(ids, vec![c, a, b]);
    }

    #[tokio::test]
    async fn remove_by_dashless_playlist_item_id_removes_member() {
        // `GetPlaylistItems` emits `PlaylistItemId` dashless (`Guid('N')`), but
        // `ChildId` is stored dashed. Removing by the dashless id the DTO exposes
        // must still hit the row (regression: it matched zero rows before).
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;

        let (mgr, _root) = playlist_manager(&db).await;

        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Mix".to_owned()),
                item_id_list: vec![track],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        // Read the entry id exactly as the wire DTO exposes it: `Guid('N')`.
        let items = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items");
        assert_eq!(items.len(), 1);
        let dashless = Uuid::parse_str(&items[0].id)
            .expect("uuid")
            .simple()
            .to_string();

        mgr.remove_item_from_playlist(&created.id, &[dashless])
            .await
            .expect("remove");

        let after = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items after");
        assert!(
            after.is_empty(),
            "removing by dashless PlaylistItemId should empty the playlist"
        );
    }

    #[tokio::test]
    async fn add_item_at_position_relocates_appended_block() {
        let db = test_db().await;
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for id in [a, b, c] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let (mgr, _root) = playlist_manager(&db).await;
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Ordered".to_owned()),
                item_id_list: vec![a, b],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        // Insert C at the front: expect [C, A, B].
        mgr.add_item_to_playlist(playlist_id, &[c], Some(0), owner)
            .await
            .expect("add at 0");
        let order: Vec<Uuid> = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert_eq!(
            order,
            vec![c, a, b],
            "position 0 should move the appended item to the front"
        );

        // No position → plain append at the end.
        let d = Uuid::new_v4();
        seed_item(&db, d, BaseItemKind::Audio).await;
        mgr.add_item_to_playlist(playlist_id, &[d], None, owner)
            .await
            .expect("append");
        let last = Uuid::parse_str(
            &mgr.get_playlist_items(playlist_id, owner)
                .await
                .expect("items2")
                .last()
                .expect("non-empty")
                .id,
        )
        .expect("uuid");
        assert_eq!(last, d, "no position should append at the end");
    }

    /// Adding, moving and removing entries leaves the playlist's `Data` alone
    /// (12.0 keeps membership out of it), so a `Data` write that would fail
    /// cannot fail an edit that committed.
    #[tokio::test]
    async fn membership_edits_never_write_data() {
        let db = test_db().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        for id in [a, b] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let (mgr, _root) = playlist_manager(&db).await;
        let owner = Uuid::new_v4();
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Members".to_owned()),
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        let data_before = crate::test_support::fetch_item(&db, playlist_id).await.data;
        crate::item_data::fail_data_writes_of(&db, playlist_id).await;

        let playlist = playlist_id.to_string();
        let order = || async {
            mgr.get_playlist_items(playlist_id, owner)
                .await
                .expect("items")
                .iter()
                .filter_map(|i| Uuid::parse_str(&i.id).ok())
                .collect::<Vec<Uuid>>()
        };
        mgr.add_item_to_playlist(playlist_id, &[a, b], None, owner)
            .await
            .expect("add");
        mgr.move_item(&playlist, &b.simple().to_string(), 0, owner)
            .await
            .expect("move");
        assert_eq!(order().await, vec![b, a]);
        mgr.remove_item_from_playlist(&playlist, &[a.simple().to_string()])
            .await
            .expect("remove");

        assert_eq!(order().await, vec![b]);
        assert_eq!(
            crate::test_support::fetch_item(&db, playlist_id).await.data,
            data_before
        );
    }

    #[tokio::test]
    async fn get_playlist_items_reflects_move_item_order() {
        // Playlists are user-ordered, so the read must echo the stored
        // `SortOrder` — including after a reorder. The member ids are FIXED and
        // ascending while the link order is their exact reverse, so any
        // id/rowid/insertion ordering (e.g. dropping `ORDER BY lc."SortOrder"`)
        // fails this deterministically rather than by luck of random guids.
        let db = test_db().await;
        let (a, b, c) = (
            Uuid::from_u128(0x1111_1111),
            Uuid::from_u128(0x2222_2222),
            Uuid::from_u128(0x3333_3333),
        );
        for id in [a, b, c] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let (mgr, _root) = playlist_manager(&db).await;
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Reordered".to_owned()),
                item_id_list: vec![c, b, a],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");
        let read = |user| {
            let mgr = mgr.clone();
            async move {
                mgr.get_playlist_items(playlist_id, user)
                    .await
                    .expect("items")
                    .iter()
                    .filter_map(|i| Uuid::parse_str(&i.id).ok())
                    .collect::<Vec<Uuid>>()
            }
        };
        assert_eq!(read(owner).await, vec![c, b, a], "link order, not id order");

        // Move the middle entry to the end: [C, A, B].
        mgr.move_item(&created.id, &b.to_string(), 2, owner)
            .await
            .expect("move");
        assert_eq!(
            read(owner).await,
            vec![c, a, b],
            "read must echo the rewritten SortOrder"
        );
    }

    #[tokio::test]
    async fn get_playlist_items_pins_entry_id_derivation() {
        // `PlaylistItemId` on the wire is the member's id in `Guid('N')` form,
        // so the row's `Id` must stay the canonical stored guid (dashed
        // uppercase) — the joined read must project the CHILD's `Id`, never the
        // playlist's.
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;
        let (mgr, _root) = playlist_manager(&db).await;
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Pinned".to_owned()),
                item_id_list: vec![track],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let items = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].id,
            ferrofin_db::store::guid_to_db(track),
            "row id must be the child's canonical stored guid"
        );
        // And the member's own fields came back (not the playlist's).
        assert_eq!(
            Some(items[0].type_.as_str()),
            crate::item_type_lookup::stored_type_name(BaseItemKind::Audio)
        );
    }

    #[tokio::test]
    async fn get_playlist_items_denies_stranger_on_populated_playlist() {
        // The single-query read decides access from columns joined onto the
        // member rows; a stranger must still get NotFound and see NO items.
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;
        let (mgr, _root) = playlist_manager(&db).await;
        let (alice, carol) = (Uuid::new_v4(), Uuid::new_v4());
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Private".to_owned()),
                item_id_list: vec![track],
                user_id: alice,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");

        // The owner sees it…
        assert_eq!(
            mgr.get_playlist_items(playlist_id, alice)
                .await
                .expect("owner reads")
                .len(),
            1
        );
        // …a stranger does not, and leaks nothing.
        let err = mgr
            .get_playlist_items(playlist_id, carol)
            .await
            .expect_err("stranger must not read a private playlist's members");
        assert!(matches!(
            err,
            ferrofin_traits::error::ServiceError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn get_playlist_items_visible_to_shared_and_open_access() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let db = test_db().await;
        // Fixed, ascending ids added in reverse: the assertion below pins link
        // order too, and can't pass by chance on an id ordering.
        let (a, b) = (Uuid::from_u128(0x4444_4444), Uuid::from_u128(0x5555_5555));
        for id in [a, b] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let (mgr, _root) = playlist_manager(&db).await;
        let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());

        // Shared read-only with bob.
        let shared = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Shared".to_owned()),
                item_id_list: vec![b, a],
                user_id: alice,
                users: vec![PlaylistUserPermissions::new(bob, false)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create shared")
            .id,
        )
        .expect("uuid");
        let ids: Vec<Uuid> = mgr
            .get_playlist_items(shared, bob)
            .await
            .expect("share grants read")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert_eq!(ids, vec![b, a], "a shared reader sees the members in order");

        // Open access: any caller reads it.
        let public = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Public".to_owned()),
                item_id_list: vec![b],
                user_id: alice,
                public: Some(true),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create public")
            .id,
        )
        .expect("uuid");
        assert_eq!(
            mgr.get_playlist_items(public, Uuid::new_v4())
                .await
                .expect("open access grants read")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn get_playlist_items_empty_playlist_still_gates_access() {
        // With no member rows the joined read has no access columns to read, so
        // the fallback must still answer: empty for the owner, NotFound for a
        // stranger.
        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let alice = Uuid::new_v4();
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Empty".to_owned()),
                user_id: alice,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");

        assert!(
            mgr.get_playlist_items(playlist_id, alice)
                .await
                .expect("owner reads an empty playlist")
                .is_empty()
        );
        assert!(matches!(
            mgr.get_playlist_items(playlist_id, Uuid::new_v4()).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_playlist_items_legacy_playlist_without_meta_row_reads() {
        // A pre-ownership playlist (no `FerrofinPlaylists` row) keeps granting
        // owner-equivalent access on the joined path — the LEFT JOIN's NULLs must
        // be read as "legacy", not "denied".
        let db = test_db().await;
        let legacy = Uuid::new_v4();
        let track = Uuid::new_v4();
        seed_item(&db, legacy, BaseItemKind::Playlist).await;
        seed_item(&db, track, BaseItemKind::Audio).await;
        let (mgr, _root) = playlist_manager(&db).await;
        mgr.add_item_to_playlist(legacy, &[track], None, Uuid::new_v4())
            .await
            .expect("add");

        let items = mgr
            .get_playlist_items(legacy, Uuid::new_v4())
            .await
            .expect("legacy playlists stay readable");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, ferrofin_db::store::guid_to_db(track));
    }

    #[tokio::test]
    async fn get_playlist_items_ignores_non_manual_links() {
        // Only manual (ChildType 0) edges are playlist members; a shortcut edge
        // on the same parent must not surface.
        let db = test_db().await;
        let member = Uuid::new_v4();
        let shortcut = Uuid::new_v4();
        for id in [member, shortcut] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let (mgr, _root) = playlist_manager(&db).await;
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Typed".to_owned()),
                item_id_list: vec![member],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");
        let links = FerrofinLinkedChildrenService::new(db.clone());
        ferrofin_traits::persistence::LinkedChildrenService::upsert_linked_child(
            &links,
            playlist_id,
            shortcut,
            1,
        )
        .await
        .expect("shortcut link");

        let ids: Vec<Uuid> = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert_eq!(ids, vec![member], "shortcut links are not playlist members");
    }

    #[tokio::test]
    async fn get_playlist_items_missing_is_not_found() {
        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let err = mgr
            .get_playlist_items(Uuid::new_v4(), Uuid::new_v4())
            .await
            .expect_err("missing");
        assert!(matches!(
            err,
            ferrofin_traits::error::ServiceError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn get_missing_playlist_is_not_found() {
        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let err = mgr
            .get_playlist_for_user(Uuid::new_v4(), Uuid::new_v4())
            .await
            .expect_err("missing");
        assert!(matches!(
            err,
            ferrofin_traits::error::ServiceError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn playlist_shares_add_update_get_remove() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;
        use ferrofin_model::playlists::PlaylistUserUpdateRequest;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Shared".to_owned()),
                user_id: Uuid::new_v4(),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        let bob = Uuid::new_v4();

        assert!(
            mgr.get_playlist_shares(playlist_id)
                .await
                .unwrap()
                .is_empty()
        );

        // Add a share.
        mgr.add_user_to_shares(&PlaylistUserUpdateRequest {
            id: playlist_id,
            user_id: bob,
            can_edit: Some(true),
        })
        .await
        .unwrap();
        let shares = mgr.get_playlist_shares(playlist_id).await.unwrap();
        assert_eq!(shares, vec![PlaylistUserPermissions::new(bob, true)]);

        // POST is idempotent and updates CanEdit.
        mgr.add_user_to_shares(&PlaylistUserUpdateRequest {
            id: playlist_id,
            user_id: bob,
            can_edit: Some(false),
        })
        .await
        .unwrap();
        let shares = mgr.get_playlist_shares(playlist_id).await.unwrap();
        assert_eq!(shares, vec![PlaylistUserPermissions::new(bob, false)]);

        // Remove it.
        mgr.remove_user_from_shares(playlist_id, bob, &PlaylistUserPermissions::new(bob, false))
            .await
            .unwrap();
        assert!(
            mgr.get_playlist_shares(playlist_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Shares keep the order they were given, as upstream's `Shares` list
    /// does: a create and an update store the request's order, a re-share
    /// moves the user to the end (`AddUserToShares`), and `Data` lists them
    /// the same way. User-id order would be amy, mid, zed.
    #[tokio::test]
    async fn playlist_shares_keep_the_order_they_were_given() {
        use ferrofin_model::entities_media::PlaylistUserPermissions as Share;
        use ferrofin_model::playlists::PlaylistUserUpdateRequest;

        async fn data_order(db: &ferrofin_db::Database, playlist: Uuid) -> Vec<String> {
            let data = crate::test_support::fetch_item(db, playlist).await.data;
            crate::item_data::parse_data(data.as_deref())["Shares"]
                .as_array()
                .expect("Shares")
                .iter()
                .map(|share| share["UserId"].as_str().expect("UserId").to_owned())
                .collect()
        }

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let (amy, mid, zed) = (
            Uuid::from_u128(0x1),
            Uuid::from_u128(0x8),
            Uuid::from_u128(0xF),
        );
        let owner = Uuid::new_v4();
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Ordered".to_owned()),
                user_id: owner,
                users: vec![Share::new(zed, false), Share::new(amy, true)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        let guids = |users: &[Uuid]| {
            users
                .iter()
                .map(|user| crate::item_data::json_guid(*user))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            mgr.get_playlist_shares(playlist_id).await.unwrap(),
            vec![Share::new(zed, false), Share::new(amy, true)]
        );
        assert_eq!(data_order(&db, playlist_id).await, guids(&[zed, amy]));

        // A re-share moves the user to the end; a new share goes after it.
        for (user, can_edit) in [(zed, true), (mid, false)] {
            mgr.add_user_to_shares(&PlaylistUserUpdateRequest {
                id: playlist_id,
                user_id: user,
                can_edit: Some(can_edit),
            })
            .await
            .unwrap();
        }
        assert_eq!(
            mgr.get_playlist_shares(playlist_id).await.unwrap(),
            vec![
                Share::new(amy, true),
                Share::new(zed, true),
                Share::new(mid, false)
            ]
        );
        assert_eq!(data_order(&db, playlist_id).await, guids(&[amy, zed, mid]));

        // An update stores the request's order.
        mgr.update_playlist(&PlaylistUpdateRequest {
            id: playlist_id,
            user_id: owner,
            users: Some(vec![Share::new(mid, true), Share::new(amy, false)]),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("update");
        assert_eq!(
            mgr.get_playlist_shares(playlist_id).await.unwrap(),
            vec![Share::new(mid, true), Share::new(amy, false)]
        );
        assert_eq!(data_order(&db, playlist_id).await, guids(&[mid, amy]));
    }

    /// A re-rate saves only the rating: a share revoked between the re-rate's
    /// read and its save stays revoked in `Data`, so the boot re-import of the
    /// shares from `Data` does not bring it back.
    #[tokio::test]
    async fn a_rerate_keeps_a_share_change_made_since_its_read() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let bob = Uuid::new_v4();
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Shared".to_owned()),
                user_id: Uuid::new_v4(),
                users: vec![PlaylistUserPermissions::new(bob, true)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        // The re-rate's read, then a revoke that lands before its save.
        let mut read = crate::test_support::fetch_item(&db, playlist_id).await;
        mgr.remove_user_from_shares(playlist_id, bob, &PlaylistUserPermissions::new(bob, true))
            .await
            .expect("revoke");
        read.official_rating = Some("PG".to_owned());
        mgr.library_manager
            .update_item_rating(read)
            .await
            .expect("re-rate");

        let stored = crate::test_support::fetch_item(&db, playlist_id).await;
        assert_eq!(stored.official_rating.as_deref(), Some("PG"));
        crate::item_data::reconcile_playlist_permissions(&db)
            .await
            .expect("boot re-import");
        assert!(
            mgr.get_playlist_shares(playlist_id)
                .await
                .expect("shares")
                .is_empty(),
            "the revoked share stays revoked after a restart"
        );
    }

    /// A whole-row save (the metadata editor's `POST /Items/{id}`) built from a
    /// row read before a share revoke keeps the revoke in `Data`, so the boot
    /// re-import of the shares from `Data` does not bring it back.
    #[tokio::test]
    async fn a_whole_row_save_keeps_a_share_change_made_since_its_read() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let bob = Uuid::new_v4();
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Shared".to_owned()),
                user_id: Uuid::new_v4(),
                users: vec![PlaylistUserPermissions::new(bob, true)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        // The editor's read, then a revoke that lands before its save.
        let mut read = crate::test_support::fetch_item(&db, playlist_id).await;
        mgr.remove_user_from_shares(playlist_id, bob, &PlaylistUserPermissions::new(bob, true))
            .await
            .expect("revoke");
        read.overview = Some("Edited".to_owned());
        mgr.library_manager
            .update_items(std::slice::from_ref(&read), None)
            .await
            .expect("save");

        let stored = crate::test_support::fetch_item(&db, playlist_id).await;
        assert_eq!(stored.overview.as_deref(), Some("Edited"));
        crate::item_data::reconcile_playlist_permissions(&db)
            .await
            .expect("boot re-import");
        assert!(
            mgr.get_playlist_shares(playlist_id)
                .await
                .expect("shares")
                .is_empty(),
            "the revoked share stays revoked after a restart"
        );
    }

    /// The same for a playlist no boot has imported yet (no
    /// `FerrofinPlaylists` row) that loses its last share: its first share
    /// change imports it, so its shares are the tables' too.
    #[tokio::test]
    async fn a_whole_row_save_keeps_a_share_change_without_an_owner_row() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;
        use ferrofin_model::playlists::PlaylistUserUpdateRequest;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let (playlist_id, bob) = (Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, playlist_id, BaseItemKind::Playlist).await;
        mgr.add_user_to_shares(&PlaylistUserUpdateRequest {
            id: playlist_id,
            user_id: bob,
            can_edit: Some(false),
        })
        .await
        .expect("share");
        let mut read = crate::test_support::fetch_item(&db, playlist_id).await;
        mgr.remove_user_from_shares(playlist_id, bob, &PlaylistUserPermissions::new(bob, false))
            .await
            .expect("revoke");
        read.overview = Some("Edited".to_owned());
        mgr.library_manager
            .update_items(std::slice::from_ref(&read), None)
            .await
            .expect("save");

        let stored = crate::test_support::fetch_item(&db, playlist_id).await;
        assert_eq!(stored.overview.as_deref(), Some("Edited"));
        crate::item_data::reconcile_playlist_permissions(&db)
            .await
            .expect("boot re-import");
        assert!(
            mgr.get_playlist_shares(playlist_id)
                .await
                .expect("shares")
                .is_empty(),
            "the revoked share stays revoked after a restart"
        );
    }

    /// An update that fails part-way applies none of its changes: its share
    /// and open-access writes never commit without their `Data` sync, so the
    /// tables and the boot re-import from `Data` agree.
    #[tokio::test]
    async fn a_failed_update_applies_none_of_its_changes() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let (bob, carol, member) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, member, BaseItemKind::Movie).await;
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Shared".to_owned()),
                item_id_list: vec![member],
                user_id: Uuid::new_v4(),
                users: vec![PlaylistUserPermissions::new(bob, true)],
                public: Some(true),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        // The `Data` sync, the update's last write, fails after the others.
        crate::item_data::fail_data_writes_of(&db, playlist_id).await;
        mgr.update_playlist(&PlaylistUpdateRequest {
            id: playlist_id,
            name: Some("Renamed".to_owned()),
            users: Some(Vec::new()),
            public: Some(false),
            ids: Some(Vec::new()),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect_err("the failed `Data` write fails the update");

        for moment in ["after the failed update", "after a restart"] {
            assert_eq!(
                mgr.get_playlist_shares(playlist_id).await.expect("shares"),
                vec![PlaylistUserPermissions::new(bob, true)],
                "the share stays {moment}"
            );
            assert!(
                mgr.get_playlist_access(playlist_id, carol).await.is_ok(),
                "the playlist stays public {moment}"
            );
            crate::item_data::reconcile_playlist_permissions(&db)
                .await
                .expect("boot re-import");
        }
        let stored = crate::test_support::fetch_item(&db, playlist_id).await;
        assert_eq!(stored.name.as_deref(), Some("Shared"));
        assert_eq!(
            super::read_entries(db.pool(), &ferrofin_db::store::guid_to_db(playlist_id))
                .await
                .expect("entries"),
            vec![(
                ferrofin_db::store::guid_to_db(member),
                i64::from(super::LINKED_CHILD_MANUAL)
            )],
            "the entry replacement rolled back"
        );
    }

    /// An update of a playlist no boot has imported yet (no
    /// `FerrofinPlaylists` row) imports it first, so its open access applies
    /// and its shares are replaced, not merged with the imported ones.
    #[tokio::test]
    async fn an_update_of_an_unimported_playlist_applies_on_its_own_rows() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let (playlist_id, bob, dave) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        crate::test_support::seed_item_with_data(
            &db,
            playlist_id,
            BaseItemKind::Playlist,
            "Unimported",
            &serde_json::json!({ "Shares": [{ "UserId": bob, "CanEdit": true }] }).to_string(),
        )
        .await;
        mgr.update_playlist(&PlaylistUpdateRequest {
            id: playlist_id,
            users: Some(vec![PlaylistUserPermissions::new(dave, false)]),
            public: Some(true),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("update");

        assert_eq!(
            mgr.get_playlist_shares(playlist_id).await.expect("shares"),
            vec![PlaylistUserPermissions::new(dave, false)]
        );
        let access = mgr
            .get_playlist_access(playlist_id, bob)
            .await
            .expect("access");
        assert!(access.open_access, "IsPublic applies");
    }

    /// Builds a playlist manager over `db` (the repeated three-collaborator
    /// constructor the access tests share), on a data directory of its own
    /// that lives as long as the returned guard, with the playlists folder
    /// startup provisions there ([`provision_root_folder`]).
    async fn playlist_manager(
        db: &ferrofin_db::Database,
    ) -> (FerrofinPlaylistManager, tempfile::TempDir) {
        let (mgr, root) = bare_playlist_manager(db);
        provision_root_folder(db, &mgr.paths).await;
        (mgr, root)
    }

    /// [`playlist_manager`] before startup has run: no playlists folder.
    fn bare_playlist_manager(
        db: &ferrofin_db::Database,
    ) -> (FerrofinPlaylistManager, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            item_repository_over(db.clone()),
            test_paths_in(root.path()),
        );
        (mgr, root)
    }

    /// What every start does before a request is served
    /// ([`crate::AggregateFolderStore::ensure`], the port of
    /// `LibraryManager.CreateRootFolder`): the aggregate root and the
    /// playlists folder a created playlist goes into, under `paths`.
    pub(super) async fn provision_root_folder(
        db: &ferrofin_db::Database,
        paths: &Arc<dyn ferrofin_traits::system::ServerApplicationPaths>,
    ) {
        let (mode, _) = super::container_identity(db, paths)
            .await
            .expect("id derivation");
        crate::AggregateFolderStore::new(
            db.clone(),
            mode,
            paths.root_folder_path(),
            paths.data_path(),
        )
        .ensure()
        .await
        .expect("provision the root folders");
    }

    #[tokio::test]
    async fn create_playlist_records_owner_and_seeds_shares() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;
        use ferrofin_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let (alice, bob, carol) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Owned".to_owned()),
                user_id: alice,
                users: vec![PlaylistUserPermissions::new(bob, true)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");
        // `playlist.Id.ToString("N")` (v12.2 PlaylistManager.cs:163).
        assert_eq!(created.id, playlist_id.simple().to_string());

        let alice_access = mgr.get_playlist_access(playlist_id, alice).await.unwrap();
        assert_eq!(alice_access.level, PlaylistAccessLevel::Owner);
        assert!(!alice_access.open_access);
        let bob_access = mgr.get_playlist_access(playlist_id, bob).await.unwrap();
        assert_eq!(bob_access.level, PlaylistAccessLevel::CanEdit);
        // The owner is not in the share list; the seeded share is.
        assert_eq!(
            mgr.get_playlist_shares(playlist_id).await.unwrap(),
            vec![PlaylistUserPermissions::new(bob, true)]
        );
        // A stranger cannot see the playlist at all.
        assert!(matches!(
            mgr.get_playlist_access(playlist_id, carol).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.get_playlist_for_user(playlist_id, carol).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
    }

    /// No user, no playlist: upstream's `GetUserById(Guid.Empty)` throws in
    /// `CreatePlaylist` and in every `GetPlaylists`/`GetPlaylistForUser`.
    #[tokio::test]
    async fn a_nil_user_is_refused() {
        use ferrofin_traits::error::ServiceError;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let invalid = |result: Result<(), ServiceError>| {
            assert!(
                matches!(result, Err(ServiceError::InvalidInput(_))),
                "{result:?}"
            );
        };
        invalid(
            mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Keyed".to_owned()),
                ..PlaylistCreationRequest::default()
            })
            .await
            .map(drop),
        );
        assert!(
            mgr.get_playlists(Uuid::new_v4()).await.unwrap().is_empty(),
            "no unowned playlist was written"
        );
        // A member, so the items read takes its joined path, not the
        // empty-playlist fallback through `access`.
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;
        let owned = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Owned".to_owned()),
                item_id_list: vec![track],
                user_id: Uuid::new_v4(),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        invalid(mgr.get_playlist_access(owned, Uuid::nil()).await.map(drop));
        invalid(
            mgr.get_playlist_for_user(owned, Uuid::nil())
                .await
                .map(drop),
        );
        invalid(mgr.get_playlist_items(owned, Uuid::nil()).await.map(drop));
        invalid(mgr.get_playlists(Uuid::nil()).await.map(drop));
    }

    #[tokio::test]
    async fn open_access_playlist_is_readable_by_anyone() {
        use ferrofin_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Public".to_owned()),
                user_id: Uuid::new_v4(),
                public: Some(true),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");

        let stranger = mgr
            .get_playlist_access(playlist_id, Uuid::new_v4())
            .await
            .expect("open playlists are visible to all");
        assert_eq!(stranger.level, PlaylistAccessLevel::Read);
        assert!(stranger.open_access);
        assert!(!stranger.level.can_edit());
    }

    #[tokio::test]
    async fn legacy_playlist_without_meta_row_grants_owner_to_all() {
        use ferrofin_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        // A pre-ownership row: the BaseItems row exists with no `FerrofinPlaylists` meta.
        let legacy = Uuid::new_v4();
        seed_item(&db, legacy, BaseItemKind::Playlist).await;

        let access = mgr
            .get_playlist_access(legacy, Uuid::new_v4())
            .await
            .expect("legacy rows stay visible");
        assert_eq!(access.level, PlaylistAccessLevel::Owner);
        assert!(!access.open_access);
        assert_eq!(mgr.get_playlists(Uuid::new_v4()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_playlists_filters_by_visibility() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let (alice, bob, carol) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let create = |name: &str, owner: Uuid, users: Vec<PlaylistUserPermissions>| {
            PlaylistCreationRequest {
                name: Some(name.to_owned()),
                user_id: owner,
                users,
                ..PlaylistCreationRequest::default()
            }
        };
        let req = create("Alice's", alice, Vec::new());
        let p1 = mgr.create_playlist(&req).await.expect("p1").id;
        let p2 = mgr
            .create_playlist(&create(
                "Bob's shared",
                bob,
                vec![PlaylistUserPermissions::new(alice, false)],
            ))
            .await
            .expect("p2")
            .id;
        mgr.create_playlist(&create("Carol's", carol, Vec::new()))
            .await
            .expect("p3");

        let p1 = Uuid::parse_str(&p1).expect("uuid");
        let p2 = Uuid::parse_str(&p2).expect("uuid");
        let visible: Vec<Uuid> = mgr
            .get_playlists(alice)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        assert_eq!(visible.len(), 2, "alice sees her own + bob's shared");
        assert!(visible.contains(&p1) && visible.contains(&p2));
    }

    #[tokio::test]
    async fn update_playlist_applies_public_and_replaces_shares() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;
        use ferrofin_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let (alice, bob, dave) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Mutable".to_owned()),
                user_id: alice,
                users: vec![PlaylistUserPermissions::new(bob, true)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");

        assert_eq!(
            crate::test_support::fetch_item(&db, playlist_id)
                .await
                .clean_name
                .as_deref(),
            Some("mutable"),
            "a new playlist is found by name"
        );
        mgr.update_playlist(&PlaylistUpdateRequest {
            id: playlist_id,
            user_id: alice,
            name: Some("Road Trip!".to_owned()),
            users: Some(vec![PlaylistUserPermissions::new(dave, false)]),
            public: Some(true),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("update");
        let renamed = crate::test_support::fetch_item(&db, playlist_id).await;
        assert_eq!(
            (renamed.name.as_deref(), renamed.clean_name.as_deref()),
            (Some("Road Trip!"), Some("road trip")),
            "a rename is found by its new name"
        );

        // The share list was replaced wholesale (bob is gone, dave read-only).
        assert_eq!(
            mgr.get_playlist_shares(playlist_id).await.unwrap(),
            vec![PlaylistUserPermissions::new(dave, false)]
        );
        // The open-access flag applied: a stranger can now read.
        let stranger = mgr
            .get_playlist_access(playlist_id, Uuid::new_v4())
            .await
            .expect("now public");
        assert_eq!(stranger.level, PlaylistAccessLevel::Read);
        assert!(stranger.open_access);
    }

    /// The stored `SortName` of row `id` (the raw row, no read-time derivation).
    async fn stored_sort_name(db: &ferrofin_db::Database, id: Uuid) -> Option<String> {
        crate::test_support::fetch_item(db, id).await.sort_name
    }

    /// C# `Name`'s setter nulls `_sortName`, so the rename's save re-derives
    /// it: from `ForcedSortName` when one is set, else `CreateSortName()`.
    #[tokio::test]
    async fn a_playlist_rename_rederives_its_sort_name() {
        let db = test_db().await;
        let (mgr, _root) = playlist_manager(&db).await;
        let alice = Uuid::new_v4();
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Mutable".to_owned()),
                user_id: alice,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        let rename = |name: &str| PlaylistUpdateRequest {
            id: playlist_id,
            user_id: alice,
            name: Some(name.to_owned()),
            ..PlaylistUpdateRequest::default()
        };

        mgr.update_playlist(&rename("The Road Trip"))
            .await
            .expect("rename");
        assert_eq!(
            stored_sort_name(&db, playlist_id).await.as_deref(),
            Some("road trip"),
            "the new name's key, not the old one's"
        );

        // A sort title outranks the name.
        let mut row = crate::test_support::fetch_item(&db, playlist_id).await;
        row.forced_sort_name = Some("Zed".to_owned());
        ferrofin_traits::persistence::ItemPersistenceService::save_items(
            &crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone()),
            std::slice::from_ref(&row),
        )
        .await
        .expect("force");
        mgr.update_playlist(&rename("Another"))
            .await
            .expect("rename");
        assert_eq!(
            stored_sort_name(&db, playlist_id).await.as_deref(),
            Some("zed")
        );
    }

    /// `Folder.AddChild` dates a new child (`DateCreated`/`DateModified`, from
    /// the new folder in `CreatePlaylist`/`CreateCollectionAsync`), and the
    /// create's `ForceSave` refresh stamps `DateLastSaved`.
    #[tokio::test]
    async fn created_box_sets_and_playlists_are_dated() {
        let db = test_db().await;
        let before = chrono::Utc::now();
        let collection = collection_manager_over(&db)
            .create_collection(&CollectionCreationOptions {
                name: "Dated Set".to_owned(),
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("collection");
        let (mgr, _root) = playlist_manager(&db).await;
        let playlist = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Dated List".to_owned()),
                user_id: Uuid::new_v4(),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("playlist")
            .id;
        let after = chrono::Utc::now();

        for id in [collection.id.as_str(), playlist.as_str()] {
            let row =
                crate::test_support::fetch_item(&db, Uuid::parse_str(id).expect("uuid")).await;
            for (column, value) in [
                ("DateCreated", row.date_created),
                ("DateModified", row.date_modified),
                ("DateLastSaved", row.date_last_saved),
            ] {
                let value = value.unwrap_or_else(|| panic!("{} has no {column}", row.type_));
                assert!(
                    (before..=after).contains(&value),
                    "{} {column} {value} is the create time",
                    row.type_
                );
            }
        }
    }

    /// Seeds a one-track playlist owned by `owner`, shared read-only with
    /// `shared_with` (when given) and open-access when `public`. Returns the
    /// manager, the playlist id and the member track id — the fixture the
    /// `get_playlist_items` visibility tests share.
    async fn playlist_with_one_track(
        db: &ferrofin_db::Database,
        owner: Uuid,
        shared_with: Option<Uuid>,
        public: bool,
    ) -> (FerrofinPlaylistManager, Uuid, Uuid, tempfile::TempDir) {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let track = Uuid::new_v4();
        seed_item(db, track, BaseItemKind::Audio).await;
        let (mgr, root) = playlist_manager(db).await;
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Members".to_owned()),
                item_id_list: vec![track],
                user_id: owner,
                users: shared_with
                    .into_iter()
                    .map(|u| PlaylistUserPermissions::new(u, false))
                    .collect(),
                public: Some(public),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        (
            mgr,
            Uuid::parse_str(&created.id).expect("uuid"),
            track,
            root,
        )
    }

    /// The member ids `get_playlist_items` returns for `reader`, in link order.
    async fn items_for(
        mgr: &FerrofinPlaylistManager,
        playlist_id: Uuid,
        reader: Uuid,
    ) -> Vec<Uuid> {
        mgr.get_playlist_items(playlist_id, reader)
            .await
            .expect("items")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect()
    }

    #[tokio::test]
    async fn get_playlist_items_readable_by_owner_and_shared_user() {
        // The members read is gated by the `access` join, whose share leg keys
        // on the *caller's* `UserId`. Owner and share-holder are the two callers
        // that leg must let through.
        let db = test_db().await;
        let (owner, friend) = (Uuid::new_v4(), Uuid::new_v4());
        let (mgr, playlist_id, track, _root) =
            playlist_with_one_track(&db, owner, Some(friend), false).await;

        assert_eq!(
            items_for(&mgr, playlist_id, owner).await,
            vec![track],
            "the owner reads their own playlist's members"
        );
        assert_eq!(
            items_for(&mgr, playlist_id, friend).await,
            vec![track],
            "a user the playlist is explicitly shared with reads its members"
        );
    }

    #[tokio::test]
    async fn get_playlist_items_hidden_from_unrelated_user() {
        // The share row belongs to `friend`; the join's `UserId` predicate is
        // the only thing standing between `stranger` and another user's private
        // playlist. Invisible reads as missing (never a partial member list).
        let db = test_db().await;
        let (owner, friend, stranger) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (mgr, playlist_id, _track, _root) =
            playlist_with_one_track(&db, owner, Some(friend), false).await;

        let err = mgr
            .get_playlist_items(playlist_id, stranger)
            .await
            .expect_err("an unrelated user must not read a shared-with-someone-else playlist");
        assert!(
            matches!(err, ferrofin_traits::error::ServiceError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        // Same verdict through the sibling read paths, so the gate can't be
        // routed around: neither exposes the playlist to a non-share-holder.
        assert!(matches!(
            mgr.get_playlist_access(playlist_id, stranger).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.get_playlist_for_user(playlist_id, stranger).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_playlist_items_open_access_readable_by_unrelated_user() {
        // Open access is orthogonal to the share join: with no share row at all
        // any caller still reads the members (unchanged behaviour).
        let db = test_db().await;
        let (mgr, playlist_id, track, _root) =
            playlist_with_one_track(&db, Uuid::new_v4(), None, true).await;

        assert_eq!(
            items_for(&mgr, playlist_id, Uuid::new_v4()).await,
            vec![track],
            "an open-access playlist stays readable by any user"
        );
    }

    /// A container's children in stored order (ordinals are dense from 0 by
    /// construction, so order is the whole story).
    async fn rows(db: &ferrofin_db::Database, parent: Uuid) -> Vec<Uuid> {
        super::read_entries(db.pool(), &ferrofin_db::store::guid_to_db(parent))
            .await
            .expect("rows")
            .into_iter()
            .map(|(c, _)| Uuid::parse_str(&c).expect("uuid"))
            .collect()
    }

    /// `Playlist.IsVisible` under a maximum parental rating, everywhere the
    /// playlist manager answers it: a playlist whose members all exceed the
    /// rating is missing (12.1 `HasParentalAllowedChild`), while the member
    /// list stays whole (`GetManageableItems`, which `GET /Playlists/{id}`
    /// reports as `ItemIds`), and a move counts the members the caller sees
    /// (`MoveItemAsync`), down past a hidden one too.
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one owner, one limited user and their playlists through every access path"
    )]
    async fn a_parental_limit_hides_playlists_and_members_beyond_it() {
        use ferrofin_db::entities::base_items::BaseItemEntity;
        use ferrofin_model::entities_media::PlaylistUserPermissions;
        use ferrofin_model::playlists::PlaylistCreationResult;
        use ferrofin_traits::error::ServiceError;
        use ferrofin_traits::library::UserManager;
        let db = test_db().await;
        let (owner, kid) = (Uuid::new_v4(), Uuid::new_v4());
        for (user, name) in [(owner, "owner"), (kid, "kid")] {
            crate::test_support::seed_named_user(&db, user, name).await;
            crate::test_support::grant_default_permissions(&db, user).await;
        }
        let users: Arc<dyn UserManager> =
            Arc::new(crate::user_manager::FerrofinUserManager::new(db.clone()));
        let mut limited = users.get_user_by_id(kid).await.expect("read").expect("kid");
        limited.max_parental_rating_score = Some(13);
        users.update_user(&limited).await.expect("limit the kid");
        let (r, pg1, pg2, pg3) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        for (id, rating) in [(r, "R"), (pg1, "PG"), (pg2, "PG"), (pg3, "PG")] {
            seed_item(&db, id, BaseItemKind::Movie).await;
            let mut row = crate::test_support::fetch_item(&db, id).await;
            row.official_rating = Some(rating.to_owned());
            crate::test_support::save_item(&db, &row).await;
        }
        let (mgr, _root) = playlist_manager(&db).await;
        let mgr = mgr.with_visibility(
            Arc::new(crate::item_visibility::ItemVisibility::new(
                db.clone(),
                item_repository_over(db.clone()),
                Arc::new(crate::LocalizationManager::new("US")),
                "/server/data".to_owned(),
            )),
            users,
        );
        let create = |name: &str, items: Vec<Uuid>| PlaylistCreationRequest {
            name: Some(name.to_owned()),
            item_id_list: items,
            user_id: owner,
            users: vec![PlaylistUserPermissions::new(kid, false)],
            ..PlaylistCreationRequest::default()
        };
        let id = |created: PlaylistCreationResult| Uuid::parse_str(&created.id).expect("uuid");
        let r_only = id(mgr.create_playlist(&create("R", vec![r])).await.expect("R"));
        let mixed = id(mgr
            .create_playlist(&create("Mixed", vec![r, pg1, pg2]))
            .await
            .expect("mixed"));
        let empty = id(mgr
            .create_playlist(&create("Empty", Vec::new()))
            .await
            .expect("empty"));

        assert!(matches!(
            mgr.get_playlist_access(r_only, kid).await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.get_playlist_items(r_only, kid).await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(
            mgr.get_playlist_access(r_only, owner).await.is_ok(),
            "no limit"
        );
        assert!(mgr.get_playlist_access(empty, kid).await.is_ok(), "empty");
        let listed = |rows: Vec<BaseItemEntity>| {
            let mut ids: Vec<Uuid> = rows
                .into_iter()
                .filter_map(|r| Uuid::parse_str(&r.id).ok())
                .collect();
            ids.sort();
            ids
        };
        let mut expected = vec![mixed, empty];
        expected.sort();
        assert_eq!(listed(mgr.get_playlists(kid).await.expect("kid")), expected);
        assert_eq!(mgr.get_playlists(owner).await.expect("owner").len(), 3);

        let members: Vec<Uuid> = mgr
            .get_playlist_items(mixed, kid)
            .await
            .expect("items")
            .into_iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        assert_eq!(members, [r, pg1, pg2], "the handler drops the R member");

        // The kid sees [pg1, pg2] and moves pg1 to their index 1: after pg2.
        mgr.move_item(&mixed.to_string(), &pg1.simple().to_string(), 1, kid)
            .await
            .expect("move");
        assert_eq!(rows(&db, mixed).await, [r, pg2, pg1]);

        // Down past a hidden member: [pg1, pg2, R, pg3] shows the kid
        // [pg1, pg2, pg3]; pg1 to index 2 lands after pg3, not before it.
        let down = id(mgr
            .create_playlist(&create("Down", vec![pg1, pg2, r, pg3]))
            .await
            .expect("down"));
        mgr.move_item(&down.to_string(), &pg1.simple().to_string(), 2, kid)
            .await
            .expect("move down");
        assert_eq!(rows(&db, down).await, [pg2, r, pg3, pg1]);
        // To the top: before the first member the kid sees, the R one kept.
        mgr.move_item(&down.to_string(), &pg1.simple().to_string(), 0, kid)
            .await
            .expect("move up");
        assert_eq!(rows(&db, down).await, [pg1, pg2, r, pg3]);
    }

    async fn seeded_playlist(
        db: &ferrofin_db::Database,
        items: &[Uuid],
    ) -> (FerrofinPlaylistManager, Uuid, Uuid, tempfile::TempDir) {
        let mut unique = items.to_vec();
        unique.sort();
        unique.dedup();
        for id in unique {
            seed_item(db, id, BaseItemKind::Audio).await;
        }
        let (mgr, root) = playlist_manager(db).await;
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Dupes".to_owned()),
                item_id_list: items.to_vec(),
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        (
            mgr,
            Uuid::parse_str(&created.id).expect("uuid"),
            owner,
            root,
        )
    }

    /// C# `AddToPlaylistInternal` (12.0) has no duplicate check: the key is
    /// `(ParentId, SortOrder)`, so the same track can sit in a playlist twice.
    #[tokio::test]
    async fn playlist_add_of_the_same_item_twice_keeps_two_entries() {
        let db = test_db().await;
        let a = Uuid::new_v4();
        let (mgr, playlist, owner, _root) = seeded_playlist(&db, &[a]).await;
        mgr.add_item_to_playlist(playlist, &[a], None, owner)
            .await
            .expect("add again");
        assert_eq!(rows(&db, playlist).await, vec![a, a]);
    }

    /// Collections go through the generic upsert, which is 12.0's
    /// find-or-append on `(ParentId, ChildId)`: never a second row.
    #[tokio::test]
    async fn collection_add_of_the_same_item_twice_keeps_one_row() {
        let db = test_db().await;
        let a = Uuid::new_v4();
        seed_item(&db, a, BaseItemKind::Movie).await;
        let mgr = collection_manager_over(&db);
        let created = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Set".to_owned(),
                item_id_list: vec![a],
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create");
        let set = Uuid::parse_str(&created.id).expect("uuid");
        mgr.add_to_collection(set, &[a]).await.expect("add again");
        assert_eq!(rows(&db, set).await, vec![a]);
    }

    /// Overlapping adds to one playlist each keep their entry: none of them
    /// starts from a list another has not finished rewriting.
    #[tokio::test]
    async fn concurrent_adds_to_a_playlist_keep_every_entry() {
        let db = test_db().await;
        let a = Uuid::new_v4();
        let (mgr, playlist, owner, _root) = seeded_playlist(&db, &[a]).await;
        let item = [a];
        let adds = (0..8).map(|_| mgr.add_item_to_playlist(playlist, &item, None, owner));
        for result in futures_util::future::join_all(adds).await {
            result.expect("add");
        }
        assert_eq!(rows(&db, playlist).await.len(), 9);
    }

    /// C# `RemoveItemFromPlaylistAsync`: every occurrence of the item goes.
    #[tokio::test]
    async fn remove_item_from_playlist_removes_every_occurrence() {
        let db = test_db().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (mgr, playlist, _, _root) = seeded_playlist(&db, &[a, b]).await;
        mgr.add_item_to_playlist(playlist, &[a], None, Uuid::nil())
            .await
            .expect("dup");
        assert_eq!(rows(&db, playlist).await.len(), 3);
        mgr.remove_item_from_playlist(&playlist.to_string(), &[a.simple().to_string()])
            .await
            .expect("remove");
        assert_eq!(rows(&db, playlist).await, vec![b]);
    }

    /// C# `MoveItemAsync`: the FIRST entry with that item id moves; ordinals
    /// are rewritten densely.
    #[tokio::test]
    async fn move_item_moves_the_first_occurrence() {
        let db = test_db().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (mgr, playlist, _, _root) = seeded_playlist(&db, &[a, b, a]).await;
        mgr.move_item(
            &playlist.to_string(),
            &a.simple().to_string(),
            2,
            Uuid::nil(),
        )
        .await
        .expect("move");
        assert_eq!(rows(&db, playlist).await, vec![b, a, a]);
    }

    /// C# `UpdatePlaylist` with `Ids` replaces the entries rather than adding.
    #[tokio::test]
    async fn update_playlist_with_ids_replaces_the_entries() {
        let db = test_db().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (mgr, playlist, _, _root) = seeded_playlist(&db, &[a, b]).await;
        mgr.update_playlist(&PlaylistUpdateRequest {
            id: playlist,
            ids: Some(vec![b, b]),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("update");
        assert_eq!(rows(&db, playlist).await, vec![b, b]);
    }

    /// A movie named "Movie" with one alternate-version link:
    /// the item the writes below aim at, and the link they must leave alone.
    async fn movie_with_a_version(db: &ferrofin_db::Database) -> (Uuid, Uuid) {
        use ferrofin_traits::persistence::LinkedChildrenService;
        let (movie, version) = (Uuid::new_v4(), Uuid::new_v4());
        crate::test_support::seed_named_item(db, movie, BaseItemKind::Movie, "Movie").await;
        seed_item(db, version, BaseItemKind::Movie).await;
        FerrofinLinkedChildrenService::new(db.clone())
            .upsert_linked_child(
                movie,
                version,
                i32::from(ferrofin_db::enums::LinkedChildType::LocalAlternateVersion),
            )
            .await
            .expect("version link");
        (movie, version)
    }

    async fn name_of(db: &ferrofin_db::Database, id: Uuid) -> String {
        library_manager_over(db.clone())
            .get_item_by_id(id)
            .await
            .expect("read")
            .expect("row")
            .name
            .unwrap_or_default()
    }

    /// Upstream's `GetPlaylistForUser` lists only playlists, so a movie or a
    /// box set id is `404` on every playlist route, and the manager's add,
    /// remove and move throw `ArgumentException`. Before, an item with no
    /// `FerrofinPlaylists` row read as a legacy playlist every caller owns:
    /// anyone could rename a movie or rewrite its version links, and read a
    /// box set's members through the playlist routes.
    #[tokio::test]
    async fn playlist_calls_refuse_an_item_that_is_no_playlist() {
        use ferrofin_traits::error::ServiceError;
        use ferrofin_traits::persistence::LinkedChildrenService;

        let db = test_db().await;
        let (movie, version) = movie_with_a_version(&db).await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;
        let (mgr, _root) = playlist_manager(&db).await;
        let caller = Uuid::new_v4();
        let movie_str = movie.to_string();

        let not_found = |result: Result<(), ServiceError>| {
            assert!(
                matches!(result, Err(ServiceError::NotFound(_))),
                "{result:?}"
            );
        };
        not_found(mgr.get_playlist_access(movie, caller).await.map(drop));
        not_found(mgr.get_playlist_for_user(movie, caller).await.map(drop));
        not_found(mgr.get_playlist_items(movie, caller).await.map(drop));
        // A box set's manual member reaches the members-present branch.
        let box_set = Uuid::new_v4();
        seed_item(&db, box_set, BaseItemKind::BoxSet).await;
        FerrofinLinkedChildrenService::new(db.clone())
            .upsert_linked_child(box_set, track, super::LINKED_CHILD_MANUAL)
            .await
            .expect("member");
        not_found(mgr.get_playlist_items(box_set, caller).await.map(drop));
        not_found(
            mgr.update_playlist(&PlaylistUpdateRequest {
                id: movie,
                name: Some("Renamed".to_owned()),
                ids: Some(vec![track]),
                ..PlaylistUpdateRequest::default()
            })
            .await,
        );

        let invalid = |result: Result<(), ServiceError>| {
            assert!(
                matches!(result, Err(ServiceError::InvalidInput(_))),
                "{result:?}"
            );
        };
        invalid(
            mgr.add_item_to_playlist(movie, &[track], None, caller)
                .await,
        );
        invalid(
            mgr.remove_item_from_playlist(&movie_str, &[version.simple().to_string()])
                .await,
        );
        invalid(
            mgr.move_item(&movie_str, &version.simple().to_string(), 0, caller)
                .await,
        );
        invalid(mgr.remove_item_from_playlist("not-a-guid", &[]).await);

        assert_eq!(name_of(&db, movie).await, "Movie");
        assert_eq!(rows(&db, movie).await, [version]);
    }

    /// `AddToCollectionAsync`/`RemoveFromCollectionAsync`: an id that is no
    /// box set's is an `ArgumentException` (`400`), and nothing changes.
    #[tokio::test]
    async fn collection_writes_refuse_an_item_that_is_no_box_set() {
        let db = test_db().await;
        let (movie, version) = movie_with_a_version(&db).await;
        let mgr = collection_manager_over(&db);
        for result in [
            mgr.add_to_collection(movie, &[version]).await,
            mgr.remove_from_collection(movie, &[version]).await,
        ] {
            assert!(
                matches!(
                    result,
                    Err(ferrofin_traits::error::ServiceError::InvalidInput(_))
                ),
                "{result:?}"
            );
        }
        assert_eq!(rows(&db, movie).await, [version]);
    }

    #[tokio::test]
    async fn plain_collection_collapse_uses_visible_folder_children_all_links_and_local_alternates()
    {
        use ferrofin_traits::persistence::LinkedChildrenService;
        let db = test_db().await;
        let temp = tempfile::tempdir().unwrap();
        let paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
            Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
                temp.path().to_str().unwrap(),
                temp.path().join("log").to_str().unwrap(),
                temp.path().join("config").to_str().unwrap(),
                temp.path().join("cache").to_str().unwrap(),
                temp.path().join("web").to_str().unwrap(),
            ));
        let linked = Arc::new(FerrofinLinkedChildrenService::new(db.clone()));
        let manager = FerrofinCollectionManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            linked.clone(),
            paths,
        );
        let mut items = Vec::new();
        let mut ids = Vec::new();
        for (kind, name) in [
            (BaseItemKind::Movie, "Movie one"),
            (BaseItemKind::Movie, "Movie shortcut"),
            (BaseItemKind::Series, "Series"),
            (BaseItemKind::Video, "Video"),
            (BaseItemKind::Movie, "Standalone"),
            (BaseItemKind::Movie, "Alternate"),
        ] {
            let id = Uuid::new_v4();
            crate::test_support::seed_named_item(&db, id, kind, name).await;
            items.push(crate::test_support::fetch_item(&db, id).await);
            ids.push(id);
        }
        assert_eq!(
            manager
                .collapse_items_within_box_sets(&items, None)
                .await
                .unwrap(),
            items,
            "missing collection folder leaves input unchanged"
        );
        let first = manager
            .create_collection(&CollectionCreationOptions {
                name: "First collection".into(),
                item_id_list: vec![ids[0], ids[2], ids[3]],
                ..Default::default()
            })
            .await
            .unwrap();
        let second = manager
            .create_collection(&CollectionCreationOptions {
                name: "Second collection".into(),
                item_id_list: vec![ids[0]],
                ..Default::default()
            })
            .await
            .unwrap();
        linked
            .upsert_linked_child(Uuid::parse_str(&second.id).unwrap(), ids[1], 1)
            .await
            .unwrap();
        linked.upsert_linked_child(ids[5], ids[4], 2).await.unwrap();
        let orphan = Uuid::new_v4();
        crate::test_support::seed_named_item(
            &db,
            orphan,
            BaseItemKind::BoxSet,
            "Orphan collection",
        )
        .await;
        linked.upsert_linked_child(orphan, ids[4], 0).await.unwrap();
        let result = manager
            .collapse_items_within_box_sets(&items, None)
            .await
            .unwrap();
        let ids_out: Vec<_> = result.iter().map(|item| item.id.clone()).collect();
        assert_eq!(ids_out.len(), 4);
        assert!(ids_out.contains(&first.id));
        assert!(ids_out.contains(&second.id));
        assert_eq!(
            result[2].id, items[3].id,
            "generic Video does not implement ISupportsBoxSetGrouping"
        );
        assert_eq!(
            result[3].id, items[4].id,
            "an orphan BoxSet outside Collections is not discovered; local alternates are not duplicated"
        );
    }

    /// The refreshes a [`QueuedRefreshes`] saw, as `(item, options)`.
    type RefreshLog =
        std::sync::Mutex<Vec<(Uuid, ferrofin_traits::providers::MetadataRefreshOptions)>>;

    /// A [`ProviderManager`] recording `queue_refresh` and
    /// `refresh_single_item` calls; nothing else on these paths reaches it.
    #[derive(Default)]
    struct QueuedRefreshes {
        queued: RefreshLog,
        awaited: RefreshLog,
    }

    /// A log's `(item, mode, force_save)` triples, emptied.
    fn drain(
        log: &RefreshLog,
    ) -> Vec<(Uuid, ferrofin_traits::providers::MetadataRefreshMode, bool)> {
        std::mem::take(&mut *log.lock().unwrap())
            .into_iter()
            .map(|(id, options)| (id, options.metadata_refresh_mode, options.force_save))
            .collect()
    }

    impl QueuedRefreshes {
        /// The queued refreshes, emptied.
        fn take(&self) -> Vec<(Uuid, ferrofin_traits::providers::MetadataRefreshMode, bool)> {
            drain(&self.queued)
        }

        /// The awaited (`refresh_single_item`) refreshes, emptied.
        fn take_awaited(
            &self,
        ) -> Vec<(Uuid, ferrofin_traits::providers::MetadataRefreshMode, bool)> {
            drain(&self.awaited)
        }
    }

    #[async_trait::async_trait]
    impl ferrofin_traits::providers::ProviderManager for QueuedRefreshes {
        async fn queue_refresh(
            &self,
            item_id: Uuid,
            options: &ferrofin_traits::providers::MetadataRefreshOptions,
            priority: ferrofin_traits::providers::RefreshPriority,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            assert_eq!(priority, ferrofin_traits::providers::RefreshPriority::High);
            self.queued.lock().unwrap().push((item_id, options.clone()));
            Ok(())
        }
        async fn refresh_full_item(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unreachable!("not on these paths")
        }
        async fn refresh_single_item(
            &self,
            item_id: Uuid,
            options: &ferrofin_traits::providers::MetadataRefreshOptions,
        ) -> Result<ferrofin_traits::providers::ItemUpdateType, ferrofin_traits::error::ServiceError>
        {
            self.awaited
                .lock()
                .unwrap()
                .push((item_id, options.clone()));
            Ok(ferrofin_traits::providers::ItemUpdateType::default())
        }
        async fn save_image_from_url(
            &self,
            _item_id: Uuid,
            _url: &str,
            _image_type: ferrofin_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unreachable!("not on these paths")
        }
        async fn save_image(
            &self,
            _item_id: Uuid,
            _content: &[u8],
            _mime_type: &str,
            _image_type: ferrofin_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unreachable!("not on these paths")
        }
        async fn get_available_remote_images(
            &self,
            _item_id: Uuid,
            _query: &ferrofin_model::providers::RemoteImageQuery,
        ) -> Result<
            Vec<ferrofin_model::providers::RemoteImageInfo>,
            ferrofin_traits::error::ServiceError,
        > {
            unreachable!("not on these paths")
        }
        async fn get_remote_image_provider_info(
            &self,
            _item_id: Uuid,
        ) -> Result<
            Vec<ferrofin_model::providers::ImageProviderInfo>,
            ferrofin_traits::error::ServiceError,
        > {
            unreachable!("not on these paths")
        }
        async fn save_metadata(
            &self,
            _item_id: Uuid,
            _update_type: ferrofin_traits::providers::ItemUpdateType,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unreachable!("not on these paths")
        }
        async fn get_external_id_infos(
            &self,
            _item_id: Uuid,
        ) -> Result<
            Vec<ferrofin_model::providers::ExternalIdInfo>,
            ferrofin_traits::error::ServiceError,
        > {
            unreachable!("not on these paths")
        }
        async fn get_all_metadata_plugins(
            &self,
        ) -> Result<
            Vec<ferrofin_model::configuration::MetadataPluginSummary>,
            ferrofin_traits::error::ServiceError,
        > {
            unreachable!("not on these paths")
        }
        async fn get_metadata_options(
            &self,
            _item_id: Uuid,
        ) -> Result<
            ferrofin_model::configuration::MetadataOptions,
            ferrofin_traits::error::ServiceError,
        > {
            unreachable!("not on these paths")
        }
        async fn get_refresh_queue(
            &self,
        ) -> Result<Vec<Uuid>, ferrofin_traits::error::ServiceError> {
            unreachable!("not on these paths")
        }
    }

    /// `CollectionManager` queues a High-priority refresh on every create and
    /// membership change: a default one for an empty create, `FullRefresh`
    /// (+`ForceSave`) for a create with items, `ForceSave` for an add that
    /// brings a new item and for every remove. An add of members already
    /// there queues nothing.
    #[tokio::test]
    async fn collection_changes_queue_the_box_set_refresh() {
        use ferrofin_traits::providers::MetadataRefreshMode::{Default, FullRefresh};
        let db = test_db().await;
        let (movie_a, movie_b) = (Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;
        let queued = Arc::new(QueuedRefreshes::default());
        let mgr = collection_manager_over(&db).with_providers(Arc::clone(&queued) as Arc<_>);

        let empty = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Empty".to_owned(),
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create empty");
        let empty = Uuid::parse_str(&empty.id).expect("uuid");
        assert_eq!(queued.take(), vec![(empty, Default, false)]);

        let full = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Full".to_owned(),
                item_id_list: vec![movie_a],
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create with items");
        let full = Uuid::parse_str(&full.id).expect("uuid");
        assert_eq!(queued.take(), vec![(full, FullRefresh, true)]);

        mgr.add_to_collection(full, &[movie_a])
            .await
            .expect("re-add");
        assert_eq!(queued.take(), Vec::new(), "nothing new, nothing queued");
        mgr.add_to_collection(full, &[movie_b]).await.expect("add");
        assert_eq!(queued.take(), vec![(full, Default, true)]);
        mgr.remove_from_collection(full, &[movie_a])
            .await
            .expect("remove");
        assert_eq!(queued.take(), vec![(full, Default, true)]);
        mgr.remove_from_collection(full, &[movie_a])
            .await
            .expect("remove a non-member");
        assert_eq!(
            queued.take(),
            vec![(full, Default, true)],
            "a remove queues its refresh whatever it removed"
        );
    }

    /// `PlaylistManager.AddToPlaylistInternal` (reached from create, update
    /// and add) and `RemoveItemFromPlaylistAsync` queue a High-priority
    /// `ForceSave` refresh; an add or create with no entries queues nothing.
    #[tokio::test]
    async fn playlist_entry_changes_queue_the_playlist_refresh() {
        use ferrofin_traits::providers::MetadataRefreshMode::Default;
        let db = test_db().await;
        let (track_a, track_b) = (Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, track_a, BaseItemKind::Audio).await;
        seed_item(&db, track_b, BaseItemKind::Audio).await;
        let queued = Arc::new(QueuedRefreshes::default());
        let (mgr, _root) = playlist_manager(&db).await;
        let mgr = mgr.with_providers(Arc::clone(&queued) as Arc<_>);
        let owner = Uuid::new_v4();

        let empty = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Empty".to_owned()),
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create empty");
        assert_eq!(queued.take(), Vec::new());
        let empty = Uuid::parse_str(&empty.id).expect("uuid");

        let mix = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Mix".to_owned()),
                item_id_list: vec![track_a],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create with items");
        let mix = Uuid::parse_str(&mix.id).expect("uuid");
        assert_eq!(queued.take(), vec![(mix, Default, true)]);

        mgr.add_item_to_playlist(empty, &[track_b], None, owner)
            .await
            .expect("add");
        assert_eq!(queued.take(), vec![(empty, Default, true)]);
        mgr.update_playlist(&PlaylistUpdateRequest {
            id: mix,
            ids: Some(vec![track_b]),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("replace entries");
        assert_eq!(queued.take(), vec![(mix, Default, true)]);
        mgr.remove_item_from_playlist(&mix.to_string(), &[track_b.simple().to_string()])
            .await
            .expect("remove");
        assert_eq!(queued.take(), vec![(mix, Default, true)]);
    }

    /// `PlaylistManager.CreatePlaylist` awaits the new playlist's first
    /// `ForceSave` refresh (`await playlist.RefreshMetadata(...)`) before it
    /// adds entries or returns, empty or not; a create with items then queues
    /// `AddToPlaylistInternal`'s refresh as well.
    #[tokio::test]
    async fn creating_a_playlist_awaits_its_first_refresh() {
        use ferrofin_traits::providers::MetadataRefreshMode::Default;
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;
        let refreshes = Arc::new(QueuedRefreshes::default());
        let (mgr, _root) = playlist_manager(&db).await;
        let mgr = mgr.with_providers(Arc::clone(&refreshes) as Arc<_>);
        let owner = Uuid::new_v4();

        let empty = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Empty".to_owned()),
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create empty");
        let empty = Uuid::parse_str(&empty.id).expect("uuid");
        assert_eq!(refreshes.take_awaited(), vec![(empty, Default, true)]);
        assert_eq!(refreshes.take(), Vec::new());

        let mix = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Mix".to_owned()),
                item_id_list: vec![track],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create with items");
        let mix = Uuid::parse_str(&mix.id).expect("uuid");
        assert_eq!(refreshes.take_awaited(), vec![(mix, Default, true)]);
        assert_eq!(refreshes.take(), vec![(mix, Default, true)]);
    }
}
