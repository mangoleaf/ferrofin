//! [`FerrofinVirtualFolderManager`] — the concrete [`VirtualFolderManager`].
//!
//! Port of the **virtual-folder** surface of
//! `Emby.Server.Implementations.Library.LibraryManager`
//! (`GetVirtualFolders`, `AddVirtualFolder`, `RemoveVirtualFolder`,
//! `AddMediaPath`, `UpdateMediaPath`, `RemoveMediaPath`, `CreateShortcut`) plus
//! the rename + library-option-update flows that live directly in
//! `LibraryStructureController`.
//!
//! A "virtual folder" is a directory under
//! [`ServerApplicationPaths::default_user_views_path`]; inside it Jellyfin keeps:
//! - one `.mblink` **shortcut** file per media path — a plain-text file whose
//!   whole content is the target path (see `MbLinkShortcutHandler`);
//! - an optional `<type>.collection` marker naming the collection type;
//! - `options.xml` — the serialized [`LibraryOptions`].
//!
//! Port simplifications, all faithful to the on-disk contract:
//! - **`options.json` instead of `options.xml`.** Ferrofin drops the C#
//!   `IXmlSerializer` everywhere and stores structured config as JSON (exactly as
//!   [`FerrofinServerConfigurationManager`](crate::FerrofinServerConfigurationManager)
//!   stores `system.json` in place of C# `system.xml`). The per-library options
//!   are therefore `options.json`; the shortcut and marker files are byte-for-byte
//!   the same as C#.
//! - **The `refresh_library` flag and the `ILibraryMonitor` stop/start dance are
//!   dropped.** The scan pipeline and filesystem watcher are later-wave
//!   subsystems, so a mutation takes effect on disk immediately and the requested
//!   refresh is a no-op (the same stance `queue_library_scan` takes today).
//! - The `ExpandVirtualPath`/`ReverseVirtualPath` network-share remapping is the
//!   identity here (no virtual-path substitution is configured at this seam).
//! - When an item store is attached ([`with_item_store`](FerrofinVirtualFolderManager::with_item_store)),
//!   add/remove also creates/deletes the library's `CollectionFolder` `BaseItem`,
//!   and `GetVirtualFolders` projects its deterministic id onto
//!   [`VirtualFolderInfo::item_id`] (so the library appears in `/UserViews` and is
//!   editable). `PrimaryImageItemId`/refresh-state still need the image + refresh
//!   queues and are left unset for now.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;

use std::sync::Arc;

use chrono::Utc;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemPersistenceService;

use crate::item_type_lookup;
use crate::user_root_folder::UserRootFolderStore;

/// The shortcut-file extension Jellyfin uses (`MbLinkShortcutHandler.Extension`).
const SHORTCUT_EXTENSION: &str = "mblink";

/// The per-library options file name (JSON counterpart of C# `options.xml`).
const OPTIONS_FILE: &str = "options.json";

/// The collection-marker file extension (`<type>.collection`).
const COLLECTION_EXTENSION: &str = "collection";

/// The concrete filesystem-backed virtual-folder manager.
///
/// Owns only the user-views root path; every operation is a directory read/write
/// beneath it, so the manager is composition-root agnostic and fully testable
/// over a temp directory.
#[derive(Clone)]
pub struct FerrofinVirtualFolderManager {
    /// The `DefaultUserViewsPath` root under which each virtual folder lives.
    root: PathBuf,
    /// The item store, set by the composition root. When present, adding/removing
    /// a library also creates/deletes its `CollectionFolder` [`BaseItemEntity`] —
    /// the row whose id is `VirtualFolderInfo.ItemId` (jellyfin-web rejects a
    /// library with a null `ItemId`) and that `/UserViews` returns. `None` in unit
    /// tests keeps the manager filesystem-only.
    persistence: Option<Arc<dyn ItemPersistenceService>>,
    /// The per-database item-id derivation mode (see
    /// [`item_type_lookup::IdDerivation`]).
    id_derivation: item_type_lookup::IdDerivation,
    /// `CollectionFolder` ids whose parent link to the `UserRootFolder` has
    /// been verified (or written) in this process. The self-heal runs on
    /// every listing, which is a hot read path; once a row is known to be
    /// parented the two extra probes (root exists, parent set) are skipped.
    parented: Arc<std::sync::Mutex<std::collections::HashSet<uuid::Uuid>>>,
    /// `{data}/playlists` — the `PlaylistsFolder` `LibraryManager.CreateRootFolder`
    /// adds to the root as a virtual child, set by the composition root. It is a
    /// root child like any library, so its location is part of
    /// [`get_physical_paths`](VirtualFolderManager::get_physical_paths). `None`
    /// in unit tests keeps the manager library-only.
    playlists_path: Option<PathBuf>,
}

impl std::fmt::Debug for FerrofinVirtualFolderManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinVirtualFolderManager")
            .field("root", &self.root)
            .field("has_item_store", &self.persistence.is_some())
            .field("id_derivation", &self.id_derivation)
            .field("playlists_path", &self.playlists_path)
            .finish_non_exhaustive()
    }
}

impl FerrofinVirtualFolderManager {
    /// Creates a manager rooted at `default_user_views_path`.
    ///
    /// The directory is created lazily on first write; a read of a missing root
    /// yields an empty folder list (a fresh server has configured no libraries).
    #[must_use]
    pub fn new(default_user_views_path: impl Into<PathBuf>) -> Self {
        Self {
            root: default_user_views_path.into(),
            persistence: None,
            id_derivation: item_type_lookup::IdDerivation::LegacyLowercase,
            parented: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            playlists_path: None,
        }
    }

    /// Attaches the item store so add/remove also creates/deletes the library's
    /// `CollectionFolder` row. Called once by the composition root.
    #[must_use]
    pub fn with_item_store(mut self, persistence: Arc<dyn ItemPersistenceService>) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Sets the per-database id-derivation mode. Called once by the
    /// composition root (unit tests keep the legacy default).
    #[must_use]
    pub fn with_id_derivation(mut self, mode: item_type_lookup::IdDerivation) -> Self {
        self.id_derivation = mode;
        self
    }

    /// Sets the `{data}/playlists` folder's path, so it is reported among the
    /// root's physical locations. Called once by the composition root with the
    /// same path the user-view manager provisions the folder at.
    #[must_use]
    pub fn with_playlists_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.playlists_path = Some(path.into());
        self
    }

    /// The deterministic `CollectionFolder` item id for a virtual-folder directory
    /// (`GetNewItemIdInternal` over the folder path) — both the created row's id
    /// and the value projected onto [`VirtualFolderInfo::item_id`]. Under the
    /// Jellyfin derivation this reproduces the ids an adopted 10.11.8 database
    /// already carries, so the self-healing upsert lands on the existing rows.
    fn collection_folder_id(&self, folder_path: &Path) -> Option<uuid::Uuid> {
        item_type_lookup::derive_item_id_with(
            &self.id_derivation,
            BaseItemKind::CollectionFolder,
            &folder_path.to_string_lossy(),
        )
    }

    /// The `UserRootFolder` provisioner over this manager's root, item store
    /// and derivation mode — the parent every `CollectionFolder` row hangs off
    /// (its directory is a child of `root/default/`, and so is its row). `None`
    /// without an item store wired.
    fn user_root(&self) -> Option<UserRootFolderStore> {
        let persistence = self.persistence.as_ref()?;
        Some(UserRootFolderStore::new(
            Arc::clone(persistence),
            self.id_derivation.clone(),
            self.root.clone(),
        ))
    }

    /// Upserts the library's `CollectionFolder` row for `folder_path` when it is
    /// missing (idempotent). No-op without an item store wired. This is the single
    /// place the row is created — on add and, self-healingly, on every
    /// [`get_virtual_folders`](VirtualFolderManager::get_virtual_folders) read — so
    /// the projected `ItemId` always backs a real row and children can parent to it
    /// without a foreign-key failure.
    ///
    /// The row's parent is the `UserRootFolder` (ensured first, as Jellyfin's
    /// `GetUserRootFolder()` does before resolving its children); a row that
    /// predates the root is re-parented in place, so the ancestor chain of
    /// every library climbs to `Items/Root` on old and adopted databases alike.
    async fn ensure_collection_folder(
        &self,
        folder_path: &Path,
        name: &str,
    ) -> Result<(), ServiceError> {
        let (Some(persistence), Some(id), Some(root)) = (
            &self.persistence,
            self.collection_folder_id(folder_path),
            self.user_root(),
        ) else {
            return Ok(());
        };
        // The library's own collection type, from its `<type>.collection`
        // marker. It has to reach the ROW: `DtoService` emits `CollectionType`
        // for every `IHasCollectionType` item on every endpoint
        // (DtoService.cs:1061-1064) and reads it off the item, not off the
        // filesystem, so a row without it serves `CollectionType: null` to
        // `/Library/MediaFolders`, `/Items`, `/Items/{id}` and
        // `/Items/{id}/Ancestors` alike.
        let collection_type = Self::read_collection_type(folder_path)
            .await
            .map(Self::collection_type_marker);
        if persistence.item_exists(id).await? {
            if self.parented.lock().is_ok_and(|set| set.contains(&id)) {
                return Ok(());
            }
            let root_id = root.ensure().await?;
            persistence.set_parent_id(id, root_id).await?;
            // Backfill for a row written before the type was stored (an older
            // Ferrofin database, or one adopted from Jellyfin mid-migration).
            // Guarded by the same in-process `parented` set, so it costs one
            // read per library per boot, not one per request.
            if let Some(marker) = collection_type {
                persistence.set_collection_type(id, marker).await?;
            }
            if let Ok(mut set) = self.parented.lock() {
                set.insert(id);
            }
            return Ok(());
        }
        let root_id = root.ensure().await?;
        let entity = BaseItemEntity {
            id: guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::CollectionFolder)
                .unwrap_or_default()
                .to_owned(),
            name: Some(name.to_owned()),
            sort_name: Some(ferrofin_util::sort_name::create_sort_name(name)),
            path: Some(folder_path.to_string_lossy().into_owned()),
            parent_id: Some(guid_to_db(root_id)),
            is_folder: true,
            date_created: Some(Utc::now()),
            data: collection_type.map(|marker| format!(r#"{{"CollectionType":"{marker}"}}"#)),
            ..BaseItemEntity::default()
        };
        persistence
            .save_items(std::slice::from_ref(&entity))
            .await?;
        if let Ok(mut set) = self.parented.lock() {
            set.insert(id);
        }
        Ok(())
    }

    /// The on-disk directory of the named virtual folder.
    fn folder_path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Sanitizes a proposed library name to a safe single path segment.
    ///
    /// Port of `IFileSystem.GetValidFilename(name.Trim())`: trims surrounding
    /// whitespace and strips the characters that are illegal in a path segment
    /// (path separators and the Windows-reserved set), so the name maps to one
    /// directory under the root.
    fn valid_filename(name: &str) -> String {
        const INVALID: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
        name.trim().replace(INVALID, "")
    }

    /// The lowercase enum name Jellyfin writes as the `<type>.collection` marker
    /// (`collectionType.ToString().ToLowerInvariant()`).
    fn collection_type_marker(collection_type: CollectionTypeOptions) -> &'static str {
        match collection_type {
            CollectionTypeOptions::movies => "movies",
            CollectionTypeOptions::tvshows => "tvshows",
            CollectionTypeOptions::music => "music",
            CollectionTypeOptions::musicvideos => "musicvideos",
            CollectionTypeOptions::homevideos => "homevideos",
            CollectionTypeOptions::boxsets => "boxsets",
            CollectionTypeOptions::books => "books",
            CollectionTypeOptions::mixed => "mixed",
        }
    }

    /// Parses a `<type>.collection` marker's stem back to a [`CollectionTypeOptions`]
    /// (case-insensitive, mirroring C# `Enum.TryParse(..., ignoreCase: true)`).
    fn parse_collection_marker(stem: &str) -> Option<CollectionTypeOptions> {
        match stem.to_ascii_lowercase().as_str() {
            "movies" => Some(CollectionTypeOptions::movies),
            "tvshows" => Some(CollectionTypeOptions::tvshows),
            "music" => Some(CollectionTypeOptions::music),
            "musicvideos" => Some(CollectionTypeOptions::musicvideos),
            "homevideos" => Some(CollectionTypeOptions::homevideos),
            "boxsets" => Some(CollectionTypeOptions::boxsets),
            "books" => Some(CollectionTypeOptions::books),
            "mixed" => Some(CollectionTypeOptions::mixed),
            _ => None,
        }
    }

    /// Maps an I/O failure to a [`ServiceError::Backend`] with context.
    fn io_err(context: &str, err: &std::io::Error) -> ServiceError {
        ServiceError::backend(format!("{context}: {err}"))
    }

    /// Reads the [`LibraryOptions`] stored in a folder's `options.json`, or the
    /// default when the file is absent or unreadable (matching C#
    /// `LoadLibraryOptions`, which falls back to `new LibraryOptions()`).
    async fn load_options(folder_path: &Path) -> LibraryOptions {
        let options_path = folder_path.join(OPTIONS_FILE);
        match tokio::fs::read(&options_path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => LibraryOptions::default(),
        }
    }

    /// Writes `options` to a folder's `options.json` (C# `SaveLibraryOptions`).
    async fn save_options(
        folder_path: &Path,
        options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        let options_path = folder_path.join(OPTIONS_FILE);
        let json = serde_json::to_vec_pretty(options)
            .map_err(|e| ServiceError::backend(format!("serialize library options: {e}")))?;
        tokio::fs::write(&options_path, json)
            .await
            .map_err(|e| Self::io_err("write options.json", &e))
    }

    /// Lists the resolved `.mblink` shortcut targets in a folder, sorted, exactly
    /// as `GetVirtualFolderInfo` builds `VirtualFolderInfo.Locations`.
    async fn resolve_locations(folder_path: &Path) -> Result<Vec<String>, ServiceError> {
        let mut locations = Vec::new();
        let mut dir = match tokio::fs::read_dir(folder_path).await {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(locations),
            Err(e) => return Err(Self::io_err("read folder", &e)),
        };
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| Self::io_err("iterate folder", &e))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some(SHORTCUT_EXTENSION)
                && let Some(target) = Self::resolve_shortcut(&path).await
            {
                locations.push(target);
            }
        }
        locations.sort();
        Ok(locations)
    }

    /// Resolves one `.mblink` shortcut to its target path (its whole text
    /// content, with a trailing separator trimmed — `MbLinkShortcutHandler.Resolve`).
    async fn resolve_shortcut(shortcut: &Path) -> Option<String> {
        let content = tokio::fs::read_to_string(shortcut).await.ok()?;
        let trimmed = content.trim_end_matches(['/', '\\']).to_owned();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    /// Reads a folder's `<type>.collection` marker, if any (`GetCollectionType`).
    async fn read_collection_type(folder_path: &Path) -> Option<CollectionTypeOptions> {
        let mut dir = tokio::fs::read_dir(folder_path).await.ok()?;
        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some(COLLECTION_EXTENSION)
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && let Some(collection_type) = Self::parse_collection_marker(stem)
            {
                return Some(collection_type);
            }
        }
        None
    }

    /// Creates a `.mblink` shortcut for `path_info` inside `folder_path`,
    /// de-duplicating the base name (C# `CreateShortcut`: append `1`s while a
    /// shortcut of that name already exists), then writing the target as plain
    /// text.
    async fn create_shortcut(
        folder_path: &Path,
        path_info: &MediaPathInfo,
    ) -> Result<(), ServiceError> {
        let target = &path_info.path;
        let mut stem = Path::new(target)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("path")
            .to_owned();
        let mut link = folder_path.join(format!("{stem}.{SHORTCUT_EXTENSION}"));
        while tokio::fs::try_exists(&link).await.unwrap_or(false) {
            stem.push('1');
            link = folder_path.join(format!("{stem}.{SHORTCUT_EXTENSION}"));
        }
        tokio::fs::write(&link, target)
            .await
            .map_err(|e| Self::io_err("write shortcut", &e))
    }

    /// Deletes the `.mblink` shortcut in `folder_path` that resolves to `target`,
    /// if one exists (C# `RemoveMediaPath` shortcut cleanup). Returns whether a
    /// shortcut was removed.
    async fn delete_shortcut_for(folder_path: &Path, target: &str) -> Result<bool, ServiceError> {
        let mut dir = tokio::fs::read_dir(folder_path)
            .await
            .map_err(|e| Self::io_err("read folder", &e))?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| Self::io_err("iterate folder", &e))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some(SHORTCUT_EXTENSION)
                && Self::resolve_shortcut(&path).await.as_deref() == Some(target)
            {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| Self::io_err("delete shortcut", &e))?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Errors when `name` does not resolve to an existing folder, else returns
    /// its path.
    async fn require_folder(&self, name: &str) -> Result<PathBuf, ServiceError> {
        let folder_path = self.folder_path(name);
        if tokio::fs::try_exists(&folder_path).await.unwrap_or(false) {
            Ok(folder_path)
        } else {
            Err(ServiceError::not_found(format!(
                "the media collection {name} does not exist"
            )))
        }
    }
}

#[async_trait]
impl crate::library_monitor::WatchRootsSource for FerrofinVirtualFolderManager {
    async fn watch_roots(&self) -> Vec<String> {
        // The roots the library monitor should watch: every location of every
        // library whose `EnableRealtimeMonitor` option is on (the per-library
        // "Enable real time monitoring" dashboard checkbox).
        match self.get_virtual_folders().await {
            Ok(folders) => folders
                .into_iter()
                .filter(|f| {
                    f.library_options
                        .as_ref()
                        .is_some_and(|o| o.enable_realtime_monitor)
                })
                .flat_map(|f| f.locations)
                .collect(),
            Err(err) => {
                tracing::warn!(%err, "failed to list libraries for watch roots");
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl VirtualFolderManager for FerrofinVirtualFolderManager {
    async fn get_virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
        let mut dir = match tokio::fs::read_dir(&self.root).await {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Self::io_err("read user-views root", &e)),
        };
        let mut folders = Vec::new();
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| Self::io_err("iterate user-views root", &e))?
        {
            let path = entry.path();
            let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
            if !is_dir {
                continue;
            }
            let name = path.file_name().and_then(|s| s.to_str()).map(str::to_owned);
            // Self-heal: the projected ItemId is deterministic and returned even
            // when no row backs it (e.g. a library created before CollectionFolder
            // rows existed). Ensure the row now, so parenting scanned children to it
            // doesn't hit a FOREIGN KEY failure and its ItemId always resolves.
            if let Some(name) = name.as_deref() {
                self.ensure_collection_folder(&path, name).await?;
            }
            folders.push(VirtualFolderInfo {
                name,
                locations: Self::resolve_locations(&path).await?,
                collection_type: Self::read_collection_type(&path).await,
                library_options: Some(Self::load_options(&path).await),
                // `ToString("N")` as `LibraryManager.GetVirtualFolderInfo` does; the
                // dashboard matches it against `RefreshProgress.ItemId` as a string.
                item_id: self
                    .collection_folder_id(&path)
                    .map(|g| g.simple().to_string()),
                // Jellyfin always reports a non-null refresh status; at rest it is
                // "Idle" (it becomes "Queued"/"Active" only while a scan is running,
                // which this manager does not track).
                refresh_status: Some("Idle".to_string()),
                ..VirtualFolderInfo::default()
            });
        }
        // Directory enumeration order is unspecified; a stable name sort keeps
        // the response deterministic (Jellyfin sorts by directory listing too).
        folders.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(folders)
    }

    /// Port of `LibraryController.GetPhysicalPaths`
    /// (`RootFolder.Children.SelectMany(c => c.PhysicalLocations)`).
    ///
    /// Overridden — rather than left on the trait default, which walks only the
    /// configured libraries — because the root's children are NOT just the
    /// libraries: `LibraryManager.CreateRootFolder` (LibraryManager.cs:1090-1120)
    /// unconditionally adds a `PlaylistsFolder` at `{data}/playlists` with
    /// `rootFolder.AddVirtualChild(folder)`, and it appears in this list. Its
    /// path is emitted whenever the composition root has wired one, not gated on
    /// the directory existing: upstream creates it in `CreateRootFolder`, before
    /// any request, so gating would make the answer depend on whether
    /// `/Library/MediaFolders` had been called first.
    ///
    /// The order is `SortName` ascending: `Folder.GetCachedChildren` issues an
    /// `InternalItemsQuery` with no `OrderBy`, and `BaseItemRepository.ApplyOrder`
    /// falls back to `query.OrderBy(e => e.SortName)`
    /// (BaseItemRepository.QueryBuilding.cs:328-331).
    async fn get_physical_paths(&self) -> Result<Vec<String>, ServiceError> {
        let mut rows: Vec<(String, Vec<String>)> = Vec::new();
        for folder in self.get_virtual_folders().await? {
            let key =
                ferrofin_util::sort_name::create_sort_name(folder.name.as_deref().unwrap_or(""));
            rows.push((key, folder.locations));
        }
        if let Some(path) = &self.playlists_path {
            rows.push((
                ferrofin_util::sort_name::create_sort_name(
                    crate::user_root_folder::PLAYLISTS_FOLDER_NAME,
                ),
                vec![path.to_string_lossy().into_owned()],
            ));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(rows.into_iter().flat_map(|(_, l)| l).collect())
    }

    async fn add_virtual_folder(
        &self,
        name: &str,
        collection_type: Option<CollectionTypeOptions>,
        options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        let name = Self::valid_filename(name);
        if name.is_empty() {
            return Err(ServiceError::invalid_input("library name cannot be empty"));
        }

        // Every configured media path must exist on disk (C# validation before
        // any side effect).
        for path_info in &options.path_infos {
            if !tokio::fs::try_exists(&path_info.path)
                .await
                .unwrap_or(false)
            {
                return Err(ServiceError::invalid_input(format!(
                    "the specified path does not exist: {}.",
                    path_info.path
                )));
            }
        }

        // De-duplicate the directory name (`Movies`, `Movies2`, `Movies3`, …).
        let mut dedup_name = name.clone();
        let mut count = 1;
        let mut folder_path = self.folder_path(&dedup_name);
        while tokio::fs::try_exists(&folder_path).await.unwrap_or(false) {
            count += 1;
            dedup_name = format!("{name}{count}");
            folder_path = self.folder_path(&dedup_name);
        }

        tokio::fs::create_dir_all(&folder_path)
            .await
            .map_err(|e| Self::io_err("create virtual folder", &e))?;

        if let Some(collection_type) = collection_type {
            let marker = folder_path.join(format!(
                "{}.{COLLECTION_EXTENSION}",
                Self::collection_type_marker(collection_type)
            ));
            tokio::fs::write(&marker, [])
                .await
                .map_err(|e| Self::io_err("write collection marker", &e))?;
        }

        Self::save_options(&folder_path, options).await?;

        for path_info in &options.path_infos {
            Self::create_shortcut(&folder_path, path_info).await?;
        }

        // Create the library's CollectionFolder item so it appears in /UserViews
        // and its ItemId resolves (jellyfin-web's library editor rejects a null
        // ItemId). Its children are populated by the library scan.
        self.ensure_collection_folder(&folder_path, &dedup_name)
            .await?;

        Ok(())
    }

    async fn remove_virtual_folder(&self, name: &str) -> Result<(), ServiceError> {
        let folder_path = self.require_folder(name).await?;
        // Delete the CollectionFolder row before the directory (its id derives from
        // the path). Child items are pruned by a later scan.
        if let (Some(persistence), Some(id)) =
            (&self.persistence, self.collection_folder_id(&folder_path))
        {
            persistence.delete_items(std::slice::from_ref(&id)).await?;
        }
        tokio::fs::remove_dir_all(&folder_path)
            .await
            .map_err(|e| Self::io_err("remove virtual folder", &e))
    }

    async fn rename_virtual_folder(&self, name: &str, new_name: &str) -> Result<(), ServiceError> {
        if name.trim().is_empty() {
            return Err(ServiceError::invalid_input("name must not be empty"));
        }
        if new_name.trim().is_empty() {
            return Err(ServiceError::invalid_input("new name must not be empty"));
        }

        let current = self.folder_path(name);
        let target = self.folder_path(new_name);

        if !tokio::fs::try_exists(&current).await.unwrap_or(false) {
            return Err(ServiceError::not_found(
                "the media collection does not exist",
            ));
        }

        let same_case_insensitive = current
            .to_string_lossy()
            .eq_ignore_ascii_case(&target.to_string_lossy());

        if !same_case_insensitive && tokio::fs::try_exists(&target).await.unwrap_or(false) {
            return Err(ServiceError::conflict(format!(
                "the media library already exists at {}.",
                target.display()
            )));
        }

        // A case-only rename hops through a temporary directory so a
        // case-insensitive filesystem does not treat it as a no-op (C# path).
        if same_case_insensitive {
            let temp = self.root.join(uuid::Uuid::new_v4().simple().to_string());
            tokio::fs::rename(&current, &temp)
                .await
                .map_err(|e| Self::io_err("rename (case hop)", &e))?;
            tokio::fs::rename(&temp, &target)
                .await
                .map_err(|e| Self::io_err("rename (case hop 2)", &e))?;
        } else {
            tokio::fs::rename(&current, &target)
                .await
                .map_err(|e| Self::io_err("rename virtual folder", &e))?;
        }

        Ok(())
    }

    async fn add_media_path(
        &self,
        virtual_folder_name: &str,
        path_info: &MediaPathInfo,
    ) -> Result<(), ServiceError> {
        if path_info.path.trim().is_empty() {
            return Err(ServiceError::invalid_input("path must not be empty"));
        }
        if !tokio::fs::try_exists(&path_info.path)
            .await
            .unwrap_or(false)
        {
            return Err(ServiceError::not_found("the path does not exist"));
        }
        let folder_path = self.require_folder(virtual_folder_name).await?;

        Self::create_shortcut(&folder_path, path_info).await?;

        let mut options = Self::load_options(&folder_path).await;
        options.path_infos.push(path_info.clone());
        Self::save_options(&folder_path, &options).await
    }

    async fn update_media_path(
        &self,
        virtual_folder_name: &str,
        path_info: &MediaPathInfo,
    ) -> Result<(), ServiceError> {
        let folder_path = self.require_folder(virtual_folder_name).await?;
        let mut options = Self::load_options(&folder_path).await;
        // Replace the matching entry (by path); append when it is new.
        if let Some(existing) = options
            .path_infos
            .iter_mut()
            .find(|i| i.path == path_info.path)
        {
            *existing = path_info.clone();
        } else {
            options.path_infos.push(path_info.clone());
        }
        Self::save_options(&folder_path, &options).await
    }

    async fn remove_media_path(
        &self,
        virtual_folder_name: &str,
        path: &str,
    ) -> Result<(), ServiceError> {
        if path.trim().is_empty() {
            return Err(ServiceError::invalid_input("path must not be empty"));
        }
        let folder_path = self.require_folder(virtual_folder_name).await?;

        Self::delete_shortcut_for(&folder_path, path).await?;

        let mut options = Self::load_options(&folder_path).await;
        options.path_infos.retain(|i| i.path != path);
        Self::save_options(&folder_path, &options).await
    }

    async fn update_library_options(
        &self,
        virtual_folder_name: &str,
        options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        let folder_path = self.require_folder(virtual_folder_name).await?;
        let existing = Self::load_options(&folder_path).await;

        // Create a shortcut for any newly-referenced media path (C# loops the
        // request's PathInfos and `CreateShortcut`s the ones not already present).
        for path_info in &options.path_infos {
            let already = existing.path_infos.iter().any(|i| i.path == path_info.path);
            if !already {
                Self::create_shortcut(&folder_path, path_info).await?;
            }
        }

        Self::save_options(&folder_path, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::FerrofinVirtualFolderManager;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use ferrofin_db::Database;
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_traits::library::VirtualFolderManager;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Builds a manager rooted at a fresh temp user-views dir; returns both so the
    /// temp dir outlives the manager.
    fn manager() -> (TempDir, FerrofinVirtualFolderManager) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinVirtualFolderManager::new(tmp.path().join("default"));
        (tmp, mgr)
    }

    /// A manager backed by a real in-memory item store, so add/remove exercises
    /// the `CollectionFolder` row create/delete.
    async fn manager_with_store() -> (TempDir, Database, FerrofinVirtualFolderManager) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let mgr = FerrofinVirtualFolderManager::new(tmp.path().join("default"))
            .with_item_store(persistence);
        (tmp, db, mgr)
    }

    #[tokio::test]
    async fn add_creates_collection_folder_row_and_projects_matching_item_id() {
        let (tmp, db, mgr) = manager_with_store().await;
        let media = media_dir(&tmp, "movies");
        mgr.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &opts_with_paths(&[media]),
        )
        .await
        .expect("add");

        let folders = mgr.get_virtual_folders().await.expect("get");
        let item_id = folders[0].item_id.clone().expect("ItemId projected");
        // The DTO projects the id in display form; the row's Id is canonical.
        let db_id = super::guid_to_db(uuid::Uuid::parse_str(&item_id).expect("uuid"));

        // Jellyfin always emits a non-null RefreshStatus; at rest it is "Idle".
        assert_eq!(folders[0].refresh_status.as_deref(), Some("Idle"));

        // The persisted CollectionFolder row exists with the projected id.
        let (type_, is_folder): (String, bool) =
            sqlx::query_as(r#"SELECT "Type", "IsFolder" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&db_id)
                .fetch_one(db.pool())
                .await
                .expect("collection folder row exists");
        assert_eq!(type_, "MediaBrowser.Controller.Entities.CollectionFolder");
        assert!(is_folder);

        // Removing the library deletes the row.
        mgr.remove_virtual_folder("Movies").await.expect("remove");
        let remaining: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&db_id)
                .fetch_one(db.pool())
                .await
                .expect("count");
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn get_virtual_folders_reheals_a_missing_collection_folder_row() {
        let (tmp, db, mgr) = manager_with_store().await;
        let media = media_dir(&tmp, "movies");
        mgr.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &opts_with_paths(&[media]),
        )
        .await
        .expect("add");
        let item_id = mgr.get_virtual_folders().await.expect("get")[0]
            .item_id
            .clone()
            .expect("ItemId");
        // The DTO projects the id in display form; the row's Id is canonical.
        let db_id = super::guid_to_db(uuid::Uuid::parse_str(&item_id).expect("uuid"));

        // Simulate a library created before CollectionFolder rows existed: the
        // on-disk folder remains but its BaseItems row is gone.
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(&db_id)
            .execute(db.writer())
            .await
            .expect("delete row");

        // Reading the folders re-heals the row (so scanned children can parent to it
        // without a foreign-key failure).
        let folders = mgr.get_virtual_folders().await.expect("get");
        assert_eq!(folders[0].item_id.as_deref(), Some(item_id.as_str()));
        let exists: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(&db_id)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(exists, 1, "the CollectionFolder row was re-created");
    }

    #[tokio::test]
    async fn collection_folder_rows_are_children_of_the_user_root_folder() {
        use crate::test_support::fetch_item;
        let (tmp, db, mgr) = manager_with_store().await;
        let media = media_dir(&tmp, "movies");
        mgr.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &opts_with_paths(&[media]),
        )
        .await
        .expect("add");
        let item_id = mgr.get_virtual_folders().await.expect("get")[0]
            .item_id
            .clone()
            .expect("ItemId");
        let library = uuid::Uuid::parse_str(&item_id).expect("uuid");
        let root_id = mgr.user_root().expect("store").id();

        // The UserRootFolder row exists (directory + row) with the derived id,
        // and the new library's row is its child.
        let root = fetch_item(&db, root_id).await;
        assert_eq!(
            root.type_,
            "MediaBrowser.Controller.Entities.UserRootFolder"
        );
        assert_eq!(root.name.as_deref(), Some("Media Folders"));
        assert_eq!(root.parent_id, None);
        let row = fetch_item(&db, library).await;
        assert_eq!(
            row.parent_id.as_deref(),
            Some(super::guid_to_db(root_id).as_str())
        );

        // A row created before the root existed (ParentId NULL) is re-parented
        // on the next listing by a fresh manager (a restarted server: the
        // parent memo is per process, so the orphan is re-checked).
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        let orphaned = ferrofin_db::entities::base_items::BaseItemEntity {
            parent_id: None,
            ..row
        };
        ferrofin_traits::persistence::ItemPersistenceService::save_items(
            &persistence,
            std::slice::from_ref(&orphaned),
        )
        .await
        .expect("orphan");
        assert_eq!(fetch_item(&db, library).await.parent_id, None);
        let restarted = FerrofinVirtualFolderManager::new(tmp.path().join("default"))
            .with_item_store(Arc::new(persistence));
        restarted.get_virtual_folders().await.expect("get");
        assert_eq!(
            fetch_item(&db, library).await.parent_id.as_deref(),
            Some(super::guid_to_db(root_id).as_str()),
            "re-parented in place"
        );
    }

    /// Creates a real on-disk media directory under `tmp` and returns its path.
    fn media_dir(tmp: &TempDir, name: &str) -> String {
        let p = tmp.path().join(name);
        std::fs::create_dir_all(&p).expect("mkdir media");
        p.to_string_lossy().into_owned()
    }

    fn opts_with_paths(paths: &[String]) -> LibraryOptions {
        LibraryOptions {
            path_infos: paths
                .iter()
                .map(|p| MediaPathInfo { path: p.clone() })
                .collect(),
            ..LibraryOptions::default()
        }
    }

    #[tokio::test]
    async fn missing_root_lists_empty() {
        let (_tmp, mgr) = manager();
        assert!(mgr.get_virtual_folders().await.unwrap().is_empty());
        assert!(mgr.get_physical_paths().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_then_get_round_trips_name_type_locations_and_options() {
        let (tmp, mgr) = manager();
        let media = media_dir(&tmp, "movies");
        let mut options = opts_with_paths(std::slice::from_ref(&media));
        options.enable_photos = true;

        mgr.add_virtual_folder("Movies", Some(CollectionTypeOptions::movies), &options)
            .await
            .unwrap();

        let folders = mgr.get_virtual_folders().await.unwrap();
        assert_eq!(folders.len(), 1);
        let folder = &folders[0];
        assert_eq!(folder.name.as_deref(), Some("Movies"));
        assert_eq!(folder.collection_type, Some(CollectionTypeOptions::movies));
        assert_eq!(folder.locations, vec![media.clone()]);
        assert!(folder.library_options.as_ref().unwrap().enable_photos);
        // Physical paths are the union of resolved locations.
        assert_eq!(mgr.get_physical_paths().await.unwrap(), vec![media]);
    }

    /// `GetPhysicalPaths` walks the ROOT's children, and
    /// `LibraryManager.CreateRootFolder` adds the `PlaylistsFolder` at
    /// `{data}/playlists` as one of them — SortName-ordered with the libraries,
    /// not appended after them.
    #[tokio::test]
    async fn physical_paths_include_the_playlists_folder_in_sort_name_order() {
        let (tmp, mgr) = manager();
        let movies = media_dir(&tmp, "movies");
        let shows = media_dir(&tmp, "shows");
        mgr.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &opts_with_paths(std::slice::from_ref(&movies)),
        )
        .await
        .unwrap();
        mgr.add_virtual_folder(
            "Shows",
            Some(CollectionTypeOptions::tvshows),
            &opts_with_paths(std::slice::from_ref(&shows)),
        )
        .await
        .unwrap();

        // Without a playlists path wired the list is libraries only.
        assert_eq!(
            mgr.get_physical_paths().await.unwrap(),
            vec![movies.clone(), shows.clone()]
        );

        let playlists = tmp.path().join("data").join("playlists");
        let mgr = mgr.with_playlists_path(&playlists);
        // Movies < Playlists < Shows by SortName, so it interleaves.
        assert_eq!(
            mgr.get_physical_paths().await.unwrap(),
            vec![
                movies,
                playlists.to_string_lossy().into_owned(),
                shows.clone(),
            ]
        );
    }

    #[tokio::test]
    async fn add_rejects_missing_media_path_and_empty_name() {
        let (_tmp, mgr) = manager();
        let bad = opts_with_paths(&["/does/not/exist".to_owned()]);
        assert!(mgr.add_virtual_folder("X", None, &bad).await.is_err());
        assert!(
            mgr.add_virtual_folder("   ", None, &LibraryOptions::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn add_deduplicates_folder_names() {
        let (tmp, mgr) = manager();
        let m1 = media_dir(&tmp, "a");
        let m2 = media_dir(&tmp, "b");
        mgr.add_virtual_folder("Lib", None, &opts_with_paths(&[m1]))
            .await
            .unwrap();
        mgr.add_virtual_folder("Lib", None, &opts_with_paths(&[m2]))
            .await
            .unwrap();
        let names: Vec<_> = mgr
            .get_virtual_folders()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["Lib".to_owned(), "Lib2".to_owned()]);
    }

    #[tokio::test]
    async fn remove_missing_is_not_found_and_present_deletes() {
        let (tmp, mgr) = manager();
        assert!(mgr.remove_virtual_folder("Nope").await.is_err());
        let media = media_dir(&tmp, "m");
        mgr.add_virtual_folder("Lib", None, &opts_with_paths(&[media]))
            .await
            .unwrap();
        mgr.remove_virtual_folder("Lib").await.unwrap();
        assert!(mgr.get_virtual_folders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_moves_folder_and_guards_conflict_and_missing() {
        let (tmp, mgr) = manager();
        let media = media_dir(&tmp, "m");
        mgr.add_virtual_folder("Old", None, &opts_with_paths(std::slice::from_ref(&media)))
            .await
            .unwrap();

        // Missing source.
        assert!(mgr.rename_virtual_folder("Ghost", "New").await.is_err());

        mgr.rename_virtual_folder("Old", "New").await.unwrap();
        let names: Vec<_> = mgr
            .get_virtual_folders()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["New".to_owned()]);

        // Conflict: renaming another lib onto an existing name.
        mgr.add_virtual_folder("Other", None, &opts_with_paths(&[media]))
            .await
            .unwrap();
        assert!(mgr.rename_virtual_folder("Other", "New").await.is_err());
    }

    #[tokio::test]
    async fn rename_case_only_succeeds() {
        let (tmp, mgr) = manager();
        let media = media_dir(&tmp, "m");
        mgr.add_virtual_folder("lib", None, &opts_with_paths(&[media]))
            .await
            .unwrap();
        mgr.rename_virtual_folder("lib", "Lib").await.unwrap();
        let names: Vec<_> = mgr
            .get_virtual_folders()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["Lib".to_owned()]);
    }

    #[tokio::test]
    async fn add_update_remove_media_path_maintain_shortcuts_and_options() {
        let (tmp, mgr) = manager();
        let m1 = media_dir(&tmp, "one");
        let m2 = media_dir(&tmp, "two");
        mgr.add_virtual_folder("Lib", None, &opts_with_paths(std::slice::from_ref(&m1)))
            .await
            .unwrap();

        // Add a second path.
        mgr.add_media_path("Lib", &MediaPathInfo { path: m2.clone() })
            .await
            .unwrap();
        let folder = mgr.get_virtual_folders().await.unwrap().remove(0);
        let mut locs = folder.locations.clone();
        locs.sort();
        let mut expected = vec![m1.clone(), m2.clone()];
        expected.sort();
        assert_eq!(locs, expected);
        assert_eq!(folder.library_options.unwrap().path_infos.len(), 2);

        // Update (idempotent replace by path).
        mgr.update_media_path("Lib", &MediaPathInfo { path: m2.clone() })
            .await
            .unwrap();
        assert_eq!(
            mgr.get_virtual_folders().await.unwrap()[0]
                .library_options
                .as_ref()
                .unwrap()
                .path_infos
                .len(),
            2
        );

        // Remove one path: its shortcut and options entry both go.
        mgr.remove_media_path("Lib", &m1).await.unwrap();
        let folder = mgr.get_virtual_folders().await.unwrap().remove(0);
        assert_eq!(folder.locations, vec![m2.clone()]);
        assert_eq!(
            folder.library_options.unwrap().path_infos,
            vec![MediaPathInfo { path: m2 }]
        );
    }

    #[tokio::test]
    async fn media_path_ops_require_existing_library() {
        let (tmp, mgr) = manager();
        let media = media_dir(&tmp, "m");
        assert!(
            mgr.add_media_path("Ghost", &MediaPathInfo { path: media })
                .await
                .is_err()
        );
        assert!(
            mgr.update_media_path(
                "Ghost",
                &MediaPathInfo {
                    path: "/x".to_owned()
                }
            )
            .await
            .is_err()
        );
        assert!(mgr.remove_media_path("Ghost", "/x").await.is_err());
    }

    #[tokio::test]
    async fn add_media_path_rejects_missing_target() {
        let (tmp, mgr) = manager();
        let media = media_dir(&tmp, "m");
        mgr.add_virtual_folder("Lib", None, &opts_with_paths(&[media]))
            .await
            .unwrap();
        assert!(
            mgr.add_media_path(
                "Lib",
                &MediaPathInfo {
                    path: "/nope".to_owned()
                }
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn update_library_options_replaces_and_creates_new_shortcuts() {
        let (tmp, mgr) = manager();
        let m1 = media_dir(&tmp, "one");
        let m2 = media_dir(&tmp, "two");
        mgr.add_virtual_folder("Lib", None, &opts_with_paths(std::slice::from_ref(&m1)))
            .await
            .unwrap();

        // New options reference an additional path → a shortcut is created for it.
        let mut new_opts = opts_with_paths(&[m1.clone(), m2.clone()]);
        new_opts.enable_realtime_monitor = true;
        mgr.update_library_options("Lib", &new_opts).await.unwrap();

        let folder = mgr.get_virtual_folders().await.unwrap().remove(0);
        let mut locs = folder.locations.clone();
        locs.sort();
        let mut expected = vec![m1, m2];
        expected.sort();
        assert_eq!(locs, expected);
        assert!(folder.library_options.unwrap().enable_realtime_monitor);
    }

    #[tokio::test]
    async fn watch_roots_returns_only_realtime_enabled_library_locations() {
        use crate::library_monitor::WatchRootsSource;
        let (tmp, mgr) = manager();
        let watched = media_dir(&tmp, "watched");
        let unwatched = media_dir(&tmp, "unwatched");
        let mut on = opts_with_paths(std::slice::from_ref(&watched));
        on.enable_realtime_monitor = true;
        mgr.add_virtual_folder("Watched", None, &on).await.unwrap();
        let mut off = opts_with_paths(&[unwatched]);
        off.enable_realtime_monitor = false;
        mgr.add_virtual_folder("Silent", None, &off).await.unwrap();

        // Only the realtime-enabled library's locations are watch roots.
        assert_eq!(mgr.watch_roots().await, vec![watched]);
    }

    #[tokio::test]
    async fn update_library_options_missing_library_is_not_found() {
        let (_tmp, mgr) = manager();
        assert!(
            mgr.update_library_options("Ghost", &LibraryOptions::default())
                .await
                .is_err()
        );
    }

    #[test]
    fn valid_filename_strips_separators_and_trims() {
        assert_eq!(
            FerrofinVirtualFolderManager::valid_filename("  a/b:c  "),
            "abc"
        );
        assert_eq!(
            FerrofinVirtualFolderManager::valid_filename("Movies"),
            "Movies"
        );
    }

    #[test]
    fn collection_marker_round_trips_every_variant() {
        for ct in [
            CollectionTypeOptions::movies,
            CollectionTypeOptions::tvshows,
            CollectionTypeOptions::music,
            CollectionTypeOptions::musicvideos,
            CollectionTypeOptions::homevideos,
            CollectionTypeOptions::boxsets,
            CollectionTypeOptions::books,
            CollectionTypeOptions::mixed,
        ] {
            let marker = FerrofinVirtualFolderManager::collection_type_marker(ct);
            assert_eq!(
                FerrofinVirtualFolderManager::parse_collection_marker(&marker.to_uppercase()),
                Some(ct)
            );
        }
        assert_eq!(
            FerrofinVirtualFolderManager::parse_collection_marker("bogus"),
            None
        );
    }
}
