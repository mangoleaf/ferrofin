//! [`DirectoryCache`]: one scan's memo of the directories it lists.
//!
//! Port of `MediaBrowser.Controller.Providers.DirectoryService` (v12.2) as
//! a library validation uses it: one instance for the whole validation
//! (`LibraryManager.cs:1499`), so the readers that ask about the same folder
//! for each of its items (the external subtitle, audio and lyric searches,
//! `MediaInfoResolver.GetExternalFiles` → `GetFilePaths`; the local image
//! providers; the NFO lookups) share one listing of it instead of listing it
//! again for every item.
//!
//! - It is a [`FileSystem`]: a listing is memoised, every other call goes to
//!   the filesystem it wraps. A listing that fails is not memoised, so the
//!   next reader tries again.
//! - [`relist`](DirectoryCache::relist) lists afresh and memoises what it
//!   read (upstream's `clearCache: true`). The plan pass lists that way: it
//!   decides from its listings what exists and what is gone, so it never
//!   reads a memo, and the readers after it read what it listed.
//! - [`invalidate`](DirectoryCache::invalidate) forgets a path and its
//!   parent (`Invalidate`): the scan does so for the artwork it saves.
//!   [`clear`](DirectoryCache::clear) forgets every listing: the scan does
//!   so after each refresh it serves, which may have written anywhere under
//!   its paths.
//! - It is bounded as upstream's is: it is dropped whole once it holds
//!   [`MAX_CACHED_RECORDS`] records (a listing counts each entry plus one) or
//!   when nothing read it for [`IDLE_TIMEOUT`] (`DropCacheIfIdleOrFull`).
//!   Upstream refreshes its access time at most once a second
//!   (`AccessIntervalMs`); this one, behind a lock anyway, on every access.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_providers::FileSystemMetadata;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};

/// `DirectoryService.MaxCachedRecords`: the records held before the whole
/// cache is dropped.
const MAX_CACHED_RECORDS: usize = 100_000;

/// `DirectoryService.IdleTimeoutMs`: how long the cache survives unread.
const IDLE_TIMEOUT: Duration = Duration::from_mins(5);

/// One scan's memo of directory listings over a [`FileSystem`].
pub(crate) struct DirectoryCache {
    file_system: Arc<dyn FileSystem>,
    state: Mutex<State>,
}

impl std::fmt::Debug for DirectoryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryCache").finish_non_exhaustive()
    }
}

/// What the cache holds.
#[derive(Default)]
struct State {
    /// `path → entries` (upstream's `_cache` and `_filePathCache`, one map:
    /// the file paths are the entries that are files).
    entries: HashMap<String, Arc<[FileSystemEntryInfo]>>,
    /// `path → entries` as the local image providers read them.
    metadata: HashMap<String, Arc<[FileSystemMetadata]>>,
    /// The records held, as upstream counts them.
    records: usize,
    /// When the cache was last read.
    last_access: Option<Instant>,
}

impl State {
    /// `DropCacheIfIdleOrFull`, at `now`.
    fn drop_if_idle_or_full(&mut self, now: Instant) {
        let idle = self
            .last_access
            .is_some_and(|last| now.duration_since(last) >= IDLE_TIMEOUT);
        if idle || self.records >= MAX_CACHED_RECORDS {
            *self = Self::default();
        }
        self.last_access = Some(now);
    }

    /// `Forget`: `path`'s records.
    fn forget(&mut self, path: &str) {
        if let Some(entries) = self.entries.remove(path) {
            self.records = self.records.saturating_sub(entries.len() + 1);
        }
        if let Some(entries) = self.metadata.remove(path) {
            self.records = self.records.saturating_sub(entries.len() + 1);
        }
    }
}

impl DirectoryCache {
    /// An empty cache over `file_system`.
    pub(crate) fn new(file_system: Arc<dyn FileSystem>) -> Self {
        Self {
            file_system,
            state: Mutex::new(State::default()),
        }
    }

    /// The state, made current; a poisoned lock still holds a usable memo.
    fn state(&self) -> MutexGuard<'_, State> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.drop_if_idle_or_full(Instant::now());
        state
    }

    /// Lists `path` afresh and memoises the listing when it succeeds
    /// (upstream's `GetFilePaths(path, clearCache: true)`).
    ///
    /// # Errors
    ///
    /// The wrapped filesystem's failure to list `path` completely; the old
    /// memo of it is forgotten either way.
    pub(crate) fn relist(&self, path: &str) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        self.state().forget(path);
        let entries = self.file_system.try_get_file_system_entries(path)?;
        self.keep(path, Arc::from(entries.as_slice()));
        Ok(entries)
    }

    /// `path`'s memoised listing, listing it first when there is none.
    fn try_shared(&self, path: &str) -> Result<Arc<[FileSystemEntryInfo]>, ServiceError> {
        if let Some(hit) = self.state().entries.get(path) {
            return Ok(Arc::clone(hit));
        }
        // Listed outside the lock: two readers of one new folder may both
        // list it, as upstream's `GetOrAdd` factory may run twice.
        let entries: Arc<[FileSystemEntryInfo]> =
            self.file_system.try_get_file_system_entries(path)?.into();
        self.keep(path, Arc::clone(&entries));
        Ok(entries)
    }

    /// Memoises `entries` as `path`'s listing.
    fn keep(&self, path: &str, entries: Arc<[FileSystemEntryInfo]>) {
        let mut state = self.state();
        state.forget(path);
        state.records += entries.len() + 1;
        state.entries.insert(path.to_owned(), entries);
    }

    /// Forgets `path` and its parent (`Invalidate`): a file written or
    /// removed there is read on the next listing.
    pub(crate) fn invalidate(&self, path: &str) {
        let mut state = self.state();
        state.forget(path);
        if let Some(parent) = Path::new(path).parent().and_then(Path::to_str) {
            state.forget(parent);
        }
    }

    /// Forgets every listing: the next reader of a folder lists it again.
    pub(crate) fn clear(&self) {
        *self.state() = State::default();
    }

    /// Whether this scan's listing of the folder `path` lies in, when it
    /// holds one, has no entry by `path`'s name (compared ignoring case, so
    /// a case-insensitive filesystem never reads a file as missing). `false`
    /// when there is no such listing: ask the filesystem.
    pub(crate) fn known_absent(&self, path: &Path) -> bool {
        let (Some(folder), Some(name)) = (
            path.parent().and_then(Path::to_str),
            path.file_name().and_then(|n| n.to_str()),
        ) else {
            return false;
        };
        let lower = name.to_lowercase();
        let same = |entry: &FileSystemEntryInfo| {
            entry.name.eq_ignore_ascii_case(name)
                || (!entry.name.is_ascii() && entry.name.to_lowercase() == lower)
        };
        self.state()
            .entries
            .get(folder)
            .is_some_and(|entries| !entries.iter().any(same))
    }
}

impl FileSystem for DirectoryCache {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        self.try_get_file_system_entries(path).unwrap_or_default()
    }

    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        self.try_shared(path).map(|entries| entries.to_vec())
    }

    fn shared_file_system_entries(&self, path: &str) -> Arc<[FileSystemEntryInfo]> {
        self.try_shared(path).unwrap_or_else(|_| Arc::from([]))
    }

    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        self.file_system.get_drives()
    }

    fn path_exists(&self, path: &str) -> bool {
        self.file_system.path_exists(path)
    }

    fn file_exists(&self, path: &str) -> bool {
        self.file_system.file_exists(path)
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.file_system.directory_exists(path)
    }

    fn validate_writable(&self, path: &str) -> Result<(), ServiceError> {
        self.file_system.validate_writable(path)
    }

    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata> {
        self.file_system.get_files(path, extensions)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        self.file_system.read_file(path)
    }
}

/// The local image providers' view of the same listings.
///
/// They read a file's length only for image files, to pass over empty ones
/// (`LocalImageProvider.GetImage`, `PopulateBackdropsFromExtraFanart`), so
/// only image files are stat'ed; every other entry reports length `0`. A
/// link counts as what it points to, as the listing classifies it.
impl ferrofin_providers::DirectoryService for DirectoryCache {
    fn file_system_entries(&self, path: &str) -> Vec<FileSystemMetadata> {
        if let Some(hit) = self.state().metadata.get(path) {
            return hit.to_vec();
        }
        let entries: Arc<[FileSystemMetadata]> = self
            .shared_file_system_entries(path)
            .iter()
            .map(image_metadata)
            .collect();
        let mut state = self.state();
        if let Some(old) = state.metadata.insert(path.to_owned(), Arc::clone(&entries)) {
            state.records = state.records.saturating_sub(old.len() + 1);
        }
        state.records += entries.len() + 1;
        entries.to_vec()
    }
}

/// `entry` as the local image providers read it (see the impl above).
fn image_metadata(entry: &FileSystemEntryInfo) -> FileSystemMetadata {
    let is_directory = entry.type_ == FileSystemEntryType::Directory;
    let extension = Path::new(&entry.path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()));
    let image = !is_directory
        && extension
            .as_deref()
            .is_some_and(ferrofin_providers::local_images::is_supported_image_extension);
    let length = if image {
        std::fs::metadata(&entry.path)
            .ok()
            .and_then(|m| i64::try_from(m.len()).ok())
            .unwrap_or(0)
    } else {
        0
    };
    FileSystemMetadata {
        exists: true,
        full_name: entry.path.clone(),
        name: entry.name.clone(),
        extension,
        length,
        last_write_time_utc: None,
        creation_time_utc: None,
        is_directory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_system::FerrofinFileSystem;
    use ferrofin_providers::DirectoryService as _;

    /// The real filesystem, counting its listings.
    #[derive(Default)]
    struct Counting {
        listings: std::sync::atomic::AtomicUsize,
    }

    impl Counting {
        fn listings(&self) -> usize {
            self.listings.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl FileSystem for Counting {
        fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
            self.try_get_file_system_entries(path).unwrap_or_default()
        }
        fn try_get_file_system_entries(
            &self,
            path: &str,
        ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
            self.listings
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            FerrofinFileSystem::new().try_get_file_system_entries(path)
        }
        fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
            Vec::new()
        }
        fn file_exists(&self, path: &str) -> bool {
            Path::new(path).is_file()
        }
        fn directory_exists(&self, path: &str) -> bool {
            Path::new(path).is_dir()
        }
        fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
            Vec::new()
        }
        fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
            std::fs::read(path).map_err(|e| ServiceError::backend(e.to_string()))
        }
    }

    fn cache() -> (tempfile::TempDir, String, Arc<Counting>, DirectoryCache) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.mkv"), b"x").unwrap();
        let dir = tmp.path().to_string_lossy().into_owned();
        let fs = Arc::new(Counting::default());
        let cache = DirectoryCache::new(fs.clone());
        (tmp, dir, fs, cache)
    }

    #[test]
    fn a_folder_is_listed_once_until_it_is_relisted_invalidated_or_cleared() {
        let (tmp, dir, fs, cache) = cache();
        assert_eq!(cache.get_file_system_entries(&dir).len(), 1);
        std::fs::write(tmp.path().join("a.srt"), b"x").unwrap();
        // The memo: the new file is not seen.
        assert_eq!(cache.get_file_system_entries(&dir).len(), 1);
        assert_eq!(fs.listings(), 1);

        assert_eq!(cache.relist(&dir).unwrap().len(), 2);
        assert_eq!(cache.get_file_system_entries(&dir).len(), 2);
        assert_eq!(fs.listings(), 2);

        std::fs::write(tmp.path().join("b.srt"), b"x").unwrap();
        // A child's invalidation forgets its folder too.
        cache.invalidate(&tmp.path().join("b.srt").to_string_lossy());
        assert_eq!(cache.get_file_system_entries(&dir).len(), 3);
        assert_eq!(fs.listings(), 3);

        std::fs::write(tmp.path().join("c.srt"), b"x").unwrap();
        cache.clear();
        assert_eq!(cache.get_file_system_entries(&dir).len(), 4);
        assert_eq!(fs.listings(), 4);
    }

    #[test]
    fn a_failed_listing_is_not_memoised() {
        let (tmp, _, fs, cache) = cache();
        let missing = tmp.path().join("later").to_string_lossy().into_owned();
        assert!(cache.try_get_file_system_entries(&missing).is_err());
        assert!(cache.get_file_system_entries(&missing).is_empty());
        std::fs::create_dir(&missing).unwrap();
        assert!(cache.try_get_file_system_entries(&missing).is_ok());
        assert_eq!(fs.listings(), 3);
    }

    #[test]
    fn known_absent_answers_only_from_a_listing() {
        let (tmp, dir, _, cache) = cache();
        let nfo = tmp.path().join("movie.nfo");
        let video = tmp.path().join("A.MKV");
        assert!(!cache.known_absent(&nfo), "nothing listed: ask the disk");
        cache.get_file_system_entries(&dir);
        assert!(cache.known_absent(&nfo));
        assert!(!cache.known_absent(&video), "names compare ignoring case");
    }

    #[test]
    fn image_entries_carry_the_image_length_and_share_the_listing() {
        let (tmp, dir, fs, cache) = cache();
        std::fs::write(tmp.path().join("poster.jpg"), b"jpeg").unwrap();
        std::fs::write(tmp.path().join("fanart.png"), b"").unwrap();
        std::fs::create_dir(tmp.path().join("extrafanart")).unwrap();
        let mut entries = cache.file_system_entries(&dir);
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let lengths: Vec<_> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.length, e.is_directory))
            .collect();
        assert_eq!(
            lengths,
            [
                ("a.mkv", 0, false),
                ("extrafanart", 0, true),
                ("fanart.png", 0, false),
                ("poster.jpg", 4, false),
            ]
        );
        assert_eq!(entries[3].extension.as_deref(), Some(".jpg"));
        cache.file_system_entries(&dir);
        cache.get_file_system_entries(&dir);
        assert_eq!(fs.listings(), 1);
    }

    #[test]
    fn the_cache_drops_whole_when_full_or_idle() {
        let start = Instant::now();
        let mut state = State {
            records: MAX_CACHED_RECORDS,
            ..State::default()
        };
        state.entries.insert("/a".to_owned(), Arc::from([]));
        state.drop_if_idle_or_full(start);
        assert!(state.entries.is_empty());
        assert_eq!(state.records, 0);

        state.entries.insert("/a".to_owned(), Arc::from([]));
        state.records = 1;
        state.drop_if_idle_or_full(start + IDLE_TIMEOUT / 2);
        assert_eq!(state.entries.len(), 1, "read again in time");
        state.drop_if_idle_or_full(start + IDLE_TIMEOUT * 2);
        assert!(state.entries.is_empty(), "idle too long");
    }
}
