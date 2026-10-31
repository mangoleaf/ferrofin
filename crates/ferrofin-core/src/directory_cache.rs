//! [`DirectoryCache`]: one scan's memo of the directories it lists.
//!
//! Port of `MediaBrowser.Controller.Providers.DirectoryService` (v12.2) as
//! a library validation uses it: one instance for the whole validation
//! (`LibraryManager.cs:1499`), so the readers that ask about the same folder
//! for each of its items (the external subtitle, audio and lyric searches,
//! `MediaInfoResolver.GetExternalFiles` → `GetFilePaths`; the local image
//! providers) share one listing of it instead of listing it again for every
//! item.
//!
//! - It is a [`FileSystem`]: a listing is memoised, every other call goes to
//!   the filesystem it wraps. A listing that fails is not memoised, so the
//!   next reader tries again.
//! - [`relist`](DirectoryCache::relist) lists afresh and memoises what it
//!   read (upstream's `clearCache: true`). The plan pass lists that way: it
//!   decides from its listings what exists and what is gone, so it never
//!   reads a memo, and the readers after it read what it listed.
//! - [`invalidate`](DirectoryCache::invalidate) forgets a path and its
//!   parent (`Invalidate`): the scan does so where it writes or deletes a
//!   file a later reader of the run lists. [`clear`](DirectoryCache::clear)
//!   forgets every listing: the scan does so after each refresh it serves,
//!   which may have written anywhere under its paths.
//! - A listing read while an invalidation or a clear landed is handed to its
//!   reader but not memoised (a generation count), so a reader that listed a
//!   folder just before a write never puts the old listing back.
//! - It is bounded as upstream's is ([`DirectoryCacheLimits`]): dropped whole
//!   once it holds `max_records` records (a listing counts each entry plus
//!   one) or when nothing read it for `idle_timeout`
//!   (`DropCacheIfIdleOrFull`). Upstream refreshes its access time at most
//!   once a second (`AccessIntervalMs`); this one, behind a lock anyway, on
//!   every access.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_providers::FileSystemMetadata;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem, OpenedFile};

/// The bounds of a [`DirectoryCache`] (`DirectoryService`'s
/// `MaxCachedRecords` and `IdleTimeoutMs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryCacheLimits {
    /// The records held before the whole cache is dropped; a listing counts
    /// each entry plus one.
    pub max_records: NonZeroUsize,
    /// How long the cache survives unread before it is dropped whole.
    pub idle_timeout: Duration,
}

impl DirectoryCacheLimits {
    /// `DirectoryService.MaxCachedRecords`.
    pub const DEFAULT_MAX_RECORDS: NonZeroUsize = NonZeroUsize::new(100_000).unwrap();
    /// `DirectoryService.IdleTimeoutMs`.
    pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_mins(5);
}

impl Default for DirectoryCacheLimits {
    /// Upstream's values.
    fn default() -> Self {
        Self {
            max_records: Self::DEFAULT_MAX_RECORDS,
            idle_timeout: Self::DEFAULT_IDLE_TIMEOUT,
        }
    }
}

/// One scan's memo of directory listings over a [`FileSystem`].
pub(crate) struct DirectoryCache {
    file_system: Arc<dyn FileSystem>,
    limits: DirectoryCacheLimits,
    state: Mutex<State>,
}

impl std::fmt::Debug for DirectoryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryCache")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
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
    /// Bumped by every invalidation, clear and drop: a listing read across
    /// one is not memoised.
    generation: u64,
}

impl State {
    /// `DropCacheIfIdleOrFull`, at `now`.
    fn drop_if_idle_or_full(&mut self, now: Instant, limits: DirectoryCacheLimits) {
        let idle = self
            .last_access
            .is_some_and(|last| now.duration_since(last) >= limits.idle_timeout);
        if idle || self.records >= limits.max_records.get() {
            self.reset();
        }
        self.last_access = Some(now);
    }

    /// Every listing forgotten.
    fn reset(&mut self) {
        *self = Self {
            generation: self.generation + 1,
            ..Self::default()
        };
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
    /// An empty cache over `file_system`, bounded by `limits`.
    pub(crate) fn new(file_system: Arc<dyn FileSystem>, limits: DirectoryCacheLimits) -> Self {
        Self {
            file_system,
            limits,
            state: Mutex::new(State::default()),
        }
    }

    /// The state, made current; a poisoned lock still holds a usable memo.
    fn state(&self) -> MutexGuard<'_, State> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.drop_if_idle_or_full(Instant::now(), self.limits);
        state
    }

    /// Lists `path` afresh and memoises the listing when it succeeds
    /// (upstream's `GetFilePaths(path, clearCache: true)`). Both of the
    /// folder's memos go, the image providers' too: upstream's `clearCache`
    /// drops only `_filePathCache`, but a fresh listing beside an old one
    /// would let two readers of one scan disagree about the folder.
    ///
    /// # Errors
    ///
    /// The wrapped filesystem's failure to list `path` completely; the old
    /// memo of it is forgotten either way.
    pub(crate) fn relist(&self, path: &str) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        let generation = {
            let mut state = self.state();
            state.forget(path);
            state.generation
        };
        let entries = self.file_system.try_get_file_system_entries(path)?;
        self.keep(path, Arc::from(entries.as_slice()), generation);
        Ok(entries)
    }

    /// `path`'s memoised listing, listing it first when there is none.
    fn try_shared(&self, path: &str) -> Result<Arc<[FileSystemEntryInfo]>, ServiceError> {
        let generation = {
            let state = self.state();
            if let Some(hit) = state.entries.get(path) {
                return Ok(Arc::clone(hit));
            }
            state.generation
        };
        // Listed outside the lock: two readers of one new folder may both
        // list it, as upstream's `GetOrAdd` factory may run twice.
        let entries: Arc<[FileSystemEntryInfo]> =
            self.file_system.try_get_file_system_entries(path)?.into();
        self.keep(path, Arc::clone(&entries), generation);
        Ok(entries)
    }

    /// Memoises `entries` as `path`'s listing, unless the cache was
    /// invalidated or cleared since `generation`, when the listing was read.
    fn keep(&self, path: &str, entries: Arc<[FileSystemEntryInfo]>, generation: u64) {
        let mut state = self.state();
        if state.generation != generation {
            return;
        }
        state.forget(path);
        state.records += entries.len() + 1;
        state.entries.insert(path.to_owned(), entries);
    }

    /// Forgets `path` and its parent (`Invalidate`): a file written or
    /// removed there is read on the next listing.
    pub(crate) fn invalidate(&self, path: &str) {
        let mut state = self.state();
        state.generation += 1;
        state.forget(path);
        if let Some(parent) = Path::new(path).parent().and_then(Path::to_str) {
            state.forget(parent);
        }
    }

    /// Forgets every listing: the next reader of a folder lists it again.
    pub(crate) fn clear(&self) {
        self.state().reset();
    }
}

/// `listed`, or no entries when the folder could not be listed (logged: the
/// readers that take this treat a failed listing as an empty folder).
fn or_empty<T>(path: &str, listed: Result<T, ServiceError>, empty: impl FnOnce() -> T) -> T {
    listed.unwrap_or_else(|err| {
        tracing::debug!(%err, path, "could not list a directory; reading it as empty");
        empty()
    })
}

impl FileSystem for DirectoryCache {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        or_empty(path, self.try_get_file_system_entries(path), Vec::new)
    }

    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        self.try_shared(path).map(|entries| entries.to_vec())
    }

    fn shared_file_system_entries(&self, path: &str) -> Arc<[FileSystemEntryInfo]> {
        or_empty(path, self.try_shared(path), || Arc::from([]))
    }

    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        self.file_system.get_drives()
    }

    fn path_exists(&self, path: &str) -> bool {
        self.file_system.path_exists(path)
    }

    fn same_file(&self, a: &str, b: &str) -> bool {
        self.file_system.same_file(a, b)
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

    fn open_file(&self, path: &str) -> Result<OpenedFile, ServiceError> {
        self.file_system.open_file(path)
    }
}

/// The local image providers' view of the same listings.
///
/// They read a file's length only for image files, to pass over empty ones
/// (`LocalImageProvider.GetImage`, `PopulateBackdropsFromExtraFanart`), so
/// only image files are stat'ed; every other entry reports length `0`. A
/// link counts as what it points to, as the listing classifies it.
impl ferrofin_providers::DirectoryService for DirectoryCache {
    fn file_system_entries(&self, path: &str) -> Arc<[FileSystemMetadata]> {
        let (hit, generation) = {
            let state = self.state();
            (state.metadata.get(path).map(Arc::clone), state.generation)
        };
        if let Some(hit) = hit {
            return hit;
        }
        let entries: Arc<[FileSystemMetadata]> = self
            .shared_file_system_entries(path)
            .iter()
            .map(image_metadata)
            .collect();
        let mut state = self.state();
        if state.generation == generation {
            if let Some(old) = state.metadata.insert(path.to_owned(), Arc::clone(&entries)) {
                state.records = state.records.saturating_sub(old.len() + 1);
            }
            state.records += entries.len() + 1;
        }
        drop(state);
        entries
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
        let cache = DirectoryCache::new(fs.clone(), DirectoryCacheLimits::default());
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
        let file_system_entries = cache.get_file_system_entries(&missing);
        assert!(file_system_entries.is_empty(), "{file_system_entries:?}");
        std::fs::create_dir(&missing).unwrap();
        assert!(cache.try_get_file_system_entries(&missing).is_ok());
        assert_eq!(fs.listings(), 3);
    }

    #[test]
    fn a_listing_read_across_an_invalidation_is_not_memoised() {
        let (tmp, dir, fs, cache) = cache();
        // A reader starts listing (the generation it saw), a writer
        // invalidates the folder, then the reader's listing lands.
        let seen = cache.state().generation;
        let stale = cache.file_system.try_get_file_system_entries(&dir).unwrap();
        std::fs::write(tmp.path().join("a.srt"), b"x").unwrap();
        cache.invalidate(&tmp.path().join("a.srt").to_string_lossy());
        cache.keep(&dir, stale.into(), seen);
        assert_eq!(
            cache.get_file_system_entries(&dir).len(),
            2,
            "listed afresh"
        );
        assert_eq!(fs.listings(), 2);
    }

    #[test]
    fn image_entries_carry_the_image_length_and_share_the_listing() {
        let (tmp, dir, fs, cache) = cache();
        std::fs::write(tmp.path().join("poster.jpg"), b"jpeg").unwrap();
        std::fs::write(tmp.path().join("fanart.png"), b"").unwrap();
        std::fs::create_dir(tmp.path().join("extrafanart")).unwrap();
        let mut entries = cache.file_system_entries(&dir).to_vec();
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
    fn image_entries_are_shared_not_copied() {
        let (_tmp, dir, fs, cache) = cache();
        let first = cache.file_system_entries(&dir);
        let second = cache.file_system_entries(&dir);
        assert!(
            Arc::ptr_eq(&first, &second),
            "a cached listing is handed out as the one shared copy"
        );
        assert_eq!(fs.listings(), 1);
    }

    #[test]
    fn the_cache_drops_whole_when_full_or_idle() {
        let limits = DirectoryCacheLimits {
            max_records: NonZeroUsize::new(3).unwrap(),
            idle_timeout: Duration::from_secs(60),
        };
        let start = Instant::now();
        let mut state = State {
            records: 3,
            ..State::default()
        };
        state.entries.insert("/a".to_owned(), Arc::from([]));
        state.drop_if_idle_or_full(start, limits);
        assert!(state.entries.is_empty());
        assert_eq!(state.records, 0);
        assert_eq!(state.generation, 1, "a drop discards listings in flight");

        state.entries.insert("/a".to_owned(), Arc::from([]));
        state.records = 1;
        state.drop_if_idle_or_full(start + Duration::from_secs(30), limits);
        assert_eq!(state.entries.len(), 1, "read again in time");
        state.drop_if_idle_or_full(start + Duration::from_secs(120), limits);
        assert!(state.entries.is_empty(), "idle too long");
    }

    #[test]
    fn the_defaults_are_upstreams() {
        let limits = DirectoryCacheLimits::default();
        assert_eq!(limits.max_records.get(), 100_000);
        assert_eq!(limits.idle_timeout, Duration::from_secs(300));
    }
}
