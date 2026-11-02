//! Cross-request caches of the rows the per-user visibility check reads on every by-id request: a
//! user's `Permissions` and `Preferences`, and the library set (every `CollectionFolder` row and
//! each library's options). They change rarely and were read several times per request (gl-973).
//!
//! Every runtime writer of those rows invalidates what it wrote, after its commit:
//! [`Database::invalidate_user_policy`], [`Database::invalidate_all_user_policies`],
//! [`Database::invalidate_libraries`] and [`Database::note_items_written`]. A read-through caller
//! takes the generation **before** its database read and stores only if no invalidation came in
//! between, so a read taken before a write is never stored after it (the auth cache's rule,
//! `ferrofin_core::auth_cache`). Entries also expire after the TTL given to
//! [`Database::enable_row_cache`] (by default [`ROW_CACHE_TTL`]), which bounds edits made outside
//! the server.
//!
//! The caches are off until [`Database::enable_row_cache`]: the composition root turns them on
//! after the boot repairs (which write these rows without invalidating), so the server and its
//! HTTP tests run with them, while unit fixtures that seed rows directly read straight through.
//!
//! [`Database`]: crate::Database
//! [`Database::invalidate_user_policy`]: crate::Database::invalidate_user_policy
//! [`Database::invalidate_all_user_policies`]: crate::Database::invalidate_all_user_policies
//! [`Database::invalidate_libraries`]: crate::Database::invalidate_libraries
//! [`Database::note_items_written`]: crate::Database::note_items_written
//! [`Database::enable_row_cache`]: crate::Database::enable_row_cache

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use ferrofin_model::configuration::LibraryOptions;
use uuid::Uuid;

use crate::entities::base_items::BaseItemEntity;

/// How long a cached entry may be served, by default, before the next read goes back to the
/// database: the auth cache's 30 s (`ferrofin_core::auth_cache::AUTH_CACHE_TTL`, a test keeps the
/// two defaults equal), so a policy edited outside the server converges as fast as the token
/// resolution that carries it. The server's `row_cache_ttl_secs` setting overrides it.
pub const ROW_CACHE_TTL: Duration = Duration::from_secs(30);

/// A user's policy rows, as stored.
#[derive(Debug, Clone, Default)]
pub struct UserPolicyRows {
    /// `Permissions` (`Kind`, `Value`).
    pub permissions: Vec<(i32, bool)>,
    /// `Preferences` (`Kind`, `Value`), the value unsplit.
    pub preferences: Vec<(i32, String)>,
}

impl UserPolicyRows {
    /// Whether a row grants the permission; a missing row reads as `false`, as C#
    /// `HasPermission` does. `Permissions` has no unique `(UserId, Kind)` index, and any row
    /// granting it counts, as the batched permission reads always counted it.
    #[must_use]
    pub fn permission(&self, kind: i32) -> bool {
        self.permissions.iter().any(|(k, v)| *k == kind && *v)
    }

    /// A preference's stored value, if it has a row.
    #[must_use]
    pub fn preference(&self, kind: i32) -> Option<&str> {
        self.preferences
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, v)| v.as_str())
    }
}

/// The library set: every `CollectionFolder` row, the rows above them (the user root and the root
/// folder), and each library's options, keyed by row id.
#[derive(Debug, Clone, Default)]
pub struct LibrarySet {
    /// The `CollectionFolder` rows.
    pub rows: Vec<BaseItemEntity>,
    /// The rows the libraries' hierarchy reaches above them, which every visibility walk ends in.
    pub ancestors: Vec<BaseItemEntity>,
    /// Each library's options, by the row's id.
    pub options: HashMap<Uuid, LibraryOptions>,
}

#[derive(Debug)]
struct Stamped<T> {
    value: Arc<T>,
    at: Instant,
}

/// The caches one [`Database`](crate::Database) handle and its clones share.
#[derive(Debug)]
pub(crate) struct RowCache {
    enabled: AtomicBool,
    /// The entries' lifetime, in milliseconds: set by [`Self::enable`].
    ttl_ms: AtomicU64,
    policy_generation: AtomicU64,
    policies: RwLock<HashMap<String, Stamped<UserPolicyRows>>>,
    library_generation: AtomicU64,
    /// Library-set reads under way ([`LibraryLoad`]): while one is, any item write may be in it.
    library_loads: AtomicUsize,
    libraries: RwLock<Option<Stamped<LibrarySet>>>,
}

/// A library-set read under way, from [`Database::begin_library_load`](crate::Database::begin_library_load):
/// take it before reading the rows and options, and [`store`](Self::store) what was read.
#[must_use = "a load stores nothing unless `store` is called"]
pub struct LibraryLoad<'a> {
    cache: &'a RowCache,
    generation: u64,
}

impl LibraryLoad<'_> {
    /// Caches `set` unless a library was written while it was read; returns it either way.
    #[must_use]
    pub fn store(self, set: LibrarySet) -> Arc<LibrarySet> {
        self.cache.put_libraries(self.generation, set)
    }
}

impl Drop for LibraryLoad<'_> {
    fn drop(&mut self) {
        self.cache.library_loads.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RowCache {
    pub(crate) fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            ttl_ms: AtomicU64::new(0),
            policy_generation: AtomicU64::new(0),
            policies: RwLock::new(HashMap::new()),
            library_generation: AtomicU64::new(0),
            library_loads: AtomicUsize::new(0),
            libraries: RwLock::new(None),
        }
    }

    pub(crate) fn enable(&self, ttl: Duration) {
        let ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        self.ttl_ms.store(ms, Ordering::SeqCst);
        self.enabled.store(true, Ordering::SeqCst);
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(self.ttl_ms.load(Ordering::SeqCst))
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub(crate) fn policy_generation(&self) -> u64 {
        self.policy_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn policy(&self, user_id: &str) -> Option<Arc<UserPolicyRows>> {
        if !self.enabled() {
            return None;
        }
        let map = self.policies.read().unwrap_or_else(PoisonError::into_inner);
        map.get(&user_id.to_ascii_lowercase())
            .filter(|hit| hit.at.elapsed() < self.ttl())
            .map(|hit| Arc::clone(&hit.value))
    }

    /// Stores `rows` unless an invalidation came after `generation` was taken; returns them either way.
    pub(crate) fn put_policy(
        &self,
        generation: u64,
        user_id: &str,
        rows: UserPolicyRows,
    ) -> Arc<UserPolicyRows> {
        let value = Arc::new(rows);
        let mut map = self
            .policies
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // Checked under the write lock: an invalidation bumps the generation before it takes the lock.
        if self.enabled() && self.policy_generation() == generation {
            map.insert(
                user_id.to_ascii_lowercase(),
                Stamped {
                    value: Arc::clone(&value),
                    at: Instant::now(),
                },
            );
        }
        value
    }

    pub(crate) fn invalidate_policy(&self, user_id: Option<&str>) {
        self.policy_generation.fetch_add(1, Ordering::SeqCst);
        let mut map = self
            .policies
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        match user_id {
            Some(id) => {
                map.remove(&id.to_ascii_lowercase());
            }
            None => map.clear(),
        }
    }

    pub(crate) fn libraries(&self) -> Option<Arc<LibrarySet>> {
        if !self.enabled() {
            return None;
        }
        let slot = self
            .libraries
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        slot.as_ref()
            .filter(|hit| hit.at.elapsed() < self.ttl())
            .map(|hit| Arc::clone(&hit.value))
    }

    /// Counted first, then the generation taken: a write that finds no load counted finished
    /// before this load's read began.
    pub(crate) fn begin_library_load(&self) -> LibraryLoad<'_> {
        self.library_loads.fetch_add(1, Ordering::SeqCst);
        LibraryLoad {
            cache: self,
            generation: self.library_generation.load(Ordering::SeqCst),
        }
    }

    fn put_libraries(&self, generation: u64, set: LibrarySet) -> Arc<LibrarySet> {
        let value = Arc::new(set);
        let mut slot = self
            .libraries
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if self.enabled() && self.library_generation.load(Ordering::SeqCst) == generation {
            *slot = Some(Stamped {
                value: Arc::clone(&value),
                at: Instant::now(),
            });
        }
        value
    }

    pub(crate) fn invalidate_libraries(&self) {
        self.library_generation.fetch_add(1, Ordering::SeqCst);
        *self
            .libraries
            .write()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// After a write to the `BaseItems` rows `ids` (stored GUID form): drops the library set when
    /// one of them is a library or a row above one. With no live set cached it only cancels the loads under way,
    /// which may have read one of those rows before the write. A write that adds a library row,
    /// or turns a row into one, calls [`Self::invalidate_libraries`] instead: its id is in no set.
    pub(crate) fn note_items_written<'a>(&self, ids: impl IntoIterator<Item = &'a str>) {
        let hit = match self.libraries() {
            None => self.library_loads.load(Ordering::SeqCst) > 0,
            Some(set) => ids.into_iter().any(|id| {
                set.rows
                    .iter()
                    .chain(&set.ancestors)
                    .any(|row| row.id.eq_ignore_ascii_case(id))
            }),
        };
        if hit {
            self.invalidate_libraries();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(ttl: Duration) -> RowCache {
        let cache = RowCache::new();
        cache.enable(ttl);
        cache
    }

    fn rows(kind: i32) -> UserPolicyRows {
        UserPolicyRows {
            permissions: vec![(kind, true)],
            preferences: vec![(kind, "a,b".into())],
        }
    }

    #[test]
    fn nothing_is_stored_or_served_until_enabled() {
        let cache = RowCache::new();
        let generation = cache.policy_generation();
        cache.put_policy(generation, "a", rows(1));
        let _ = cache.begin_library_load().store(LibrarySet::default());
        assert!(cache.policy("a").is_none() && cache.libraries().is_none());
    }

    #[test]
    fn a_policy_read_taken_before_an_invalidation_is_not_stored() {
        let cache = enabled(ROW_CACHE_TTL);
        let generation = cache.policy_generation();
        cache.invalidate_policy(Some("U1"));
        let served = cache.put_policy(generation, "U1", rows(1));
        assert!(served.permission(1), "the caller still gets what it read");
        assert!(cache.policy("u1").is_none(), "but it is not stored");
        let generation = cache.policy_generation();
        cache.put_policy(generation, "U1", rows(2));
        assert!(
            cache.policy("u1").is_some_and(|p| p.permission(2)),
            "keys ignore case"
        );
        assert!(!cache.policy("U1").is_some_and(|p| p.permission(1)));
    }

    #[test]
    fn any_row_granting_a_permission_counts() {
        let policy = UserPolicyRows {
            permissions: vec![(1, false), (1, true)],
            preferences: vec![],
        };
        assert!(policy.permission(1));
        assert!(!policy.permission(2));
    }

    #[test]
    fn invalidating_one_user_keeps_the_others_and_none_clears_all() {
        let cache = enabled(ROW_CACHE_TTL);
        for user in ["a", "b"] {
            let generation = cache.policy_generation();
            cache.put_policy(generation, user, rows(1));
        }
        cache.invalidate_policy(Some("A"));
        assert!(cache.policy("a").is_none() && cache.policy("b").is_some());
        cache.invalidate_policy(None);
        assert!(cache.policy("b").is_none());
    }

    #[test]
    fn entries_expire() {
        let cache = enabled(Duration::ZERO);
        let generation = cache.policy_generation();
        cache.put_policy(generation, "a", rows(1));
        assert!(cache.policy("a").is_none());
        let _ = cache.begin_library_load().store(LibrarySet::default());
        assert!(cache.libraries().is_none());
    }

    #[test]
    fn a_write_to_a_library_row_drops_the_set_and_a_write_elsewhere_keeps_it() {
        let cache = enabled(ROW_CACHE_TTL);
        let library = BaseItemEntity {
            id: "AAAA".into(),
            ..BaseItemEntity::default()
        };
        let _ = cache.begin_library_load().store(LibrarySet {
            rows: vec![library],
            ..LibrarySet::default()
        });
        cache.note_items_written(["bbbb"]);
        assert!(cache.libraries().is_some(), "another row: kept");
        cache.note_items_written(["aaaa"]);
        assert!(cache.libraries().is_none(), "the library's row: dropped");
        let root = BaseItemEntity {
            id: "CCCC".into(),
            ..BaseItemEntity::default()
        };
        let _ = cache.begin_library_load().store(LibrarySet {
            ancestors: vec![root],
            ..LibrarySet::default()
        });
        cache.note_items_written(["cccc"]);
        assert!(
            cache.libraries().is_none(),
            "a row above the libraries: dropped"
        );
    }

    #[test]
    fn a_write_during_a_load_keeps_that_load_out_and_one_before_it_does_not() {
        let cache = enabled(ROW_CACHE_TTL);
        let load = cache.begin_library_load();
        cache.note_items_written(["anything"]);
        let _ = load.store(LibrarySet::default());
        assert!(cache.libraries().is_none(), "written while it read");
        cache.note_items_written(["anything"]);
        let _ = cache.begin_library_load().store(LibrarySet::default());
        assert!(cache.libraries().is_some(), "written before it began");
    }

    #[test]
    fn a_load_that_ends_without_storing_stops_counting() {
        let cache = enabled(ROW_CACHE_TTL);
        drop(cache.begin_library_load());
        let generation = cache.library_generation.load(Ordering::SeqCst);
        cache.note_items_written(["anything"]);
        assert_eq!(cache.library_generation.load(Ordering::SeqCst), generation);
    }
}
