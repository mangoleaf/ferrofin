//! [`HermitVirtualFolderManager`] — the concrete [`VirtualFolderManager`].
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
//! - **`options.json` instead of `options.xml`.** Hermit drops the C#
//!   `IXmlSerializer` everywhere and stores structured config as JSON (exactly as
//!   [`HermitServerConfigurationManager`](crate::HermitServerConfigurationManager)
//!   stores `system.json` in place of C# `system.xml`). The per-library options
//!   are therefore `options.json`; the shortcut and marker files are byte-for-byte
//!   the same as C#.
//! - **The `refresh_library` flag and the `ILibraryMonitor` stop/start dance are
//!   dropped.** The scan pipeline and filesystem watcher are later-wave
//!   subsystems, so a mutation takes effect on disk immediately and the requested
//!   refresh is a no-op (the same stance `queue_library_scan` takes today).
//! - The `ExpandVirtualPath`/`ReverseVirtualPath` network-share remapping is the
//!   identity here (no virtual-path substitution is configured at this seam).
//! - `ItemId`/`PrimaryImageItemId`/refresh-state on [`VirtualFolderInfo`] need the
//!   DB-backed collection-folder rows + refresh queue, which are not modeled here,
//!   so they are left unset (Jellyfin leaves them null for a not-yet-materialized
//!   folder).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hermit_model::configuration::{LibraryOptions, MediaPathInfo};
use hermit_model::entities::CollectionTypeOptions;
use hermit_model::entities_media::VirtualFolderInfo;

use hermit_traits::error::ServiceError;
use hermit_traits::library::VirtualFolderManager;

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
#[derive(Debug, Clone)]
pub struct HermitVirtualFolderManager {
    /// The `DefaultUserViewsPath` root under which each virtual folder lives.
    root: PathBuf,
}

impl HermitVirtualFolderManager {
    /// Creates a manager rooted at `default_user_views_path`.
    ///
    /// The directory is created lazily on first write; a read of a missing root
    /// yields an empty folder list (a fresh server has configured no libraries).
    #[must_use]
    pub fn new(default_user_views_path: impl Into<PathBuf>) -> Self {
        Self {
            root: default_user_views_path.into(),
        }
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
impl VirtualFolderManager for HermitVirtualFolderManager {
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
            folders.push(VirtualFolderInfo {
                name,
                locations: Self::resolve_locations(&path).await?,
                collection_type: Self::read_collection_type(&path).await,
                library_options: Some(Self::load_options(&path).await),
                ..VirtualFolderInfo::default()
            });
        }
        // Directory enumeration order is unspecified; a stable name sort keeps
        // the response deterministic (Jellyfin sorts by directory listing too).
        folders.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(folders)
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

        Ok(())
    }

    async fn remove_virtual_folder(&self, name: &str) -> Result<(), ServiceError> {
        let folder_path = self.require_folder(name).await?;
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
    use super::HermitVirtualFolderManager;
    use hermit_model::configuration::{LibraryOptions, MediaPathInfo};
    use hermit_model::entities::CollectionTypeOptions;
    use hermit_traits::library::VirtualFolderManager;
    use tempfile::TempDir;

    /// Builds a manager rooted at a fresh temp user-views dir; returns both so the
    /// temp dir outlives the manager.
    fn manager() -> (TempDir, HermitVirtualFolderManager) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = HermitVirtualFolderManager::new(tmp.path().join("default"));
        (tmp, mgr)
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
            HermitVirtualFolderManager::valid_filename("  a/b:c  "),
            "abc"
        );
        assert_eq!(
            HermitVirtualFolderManager::valid_filename("Movies"),
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
            let marker = HermitVirtualFolderManager::collection_type_marker(ct);
            assert_eq!(
                HermitVirtualFolderManager::parse_collection_marker(&marker.to_uppercase()),
                Some(ct)
            );
        }
        assert_eq!(
            HermitVirtualFolderManager::parse_collection_marker("bogus"),
            None
        );
    }
}
