//! [`KeyedLocks`] — the port of Jellyfin's `AsyncKeyedLocker<string>`.
//!
//! Several ffmpeg paths must serialize work that shares a key — an output cache
//! path, a subtitle conversion stream key, an attachment folder, an HLS playlist
//! — so two callers never race two ffmpeg processes onto the same output files.
//! The C# does this with `AsyncKeyedLocker<string>`, whose defining property is
//! that it is **reference-counted**: the entry for a key exists only while some
//! caller holds it, and disappears when the last holder releases.
//!
//! That property is the reason this type exists rather than a bare
//! `Mutex<HashMap<String, Arc<Mutex<()>>>>`. The keys here are per-session output
//! paths, not a fixed vocabulary, so a map that only ever inserts grows for the
//! life of the process: one entry per transcode session, per subtitle file, per
//! attachment folder, forever. Small individually, unbounded collectively, and
//! invisible until a server has been up for weeks.
//!
//! # How eviction stays correct
//!
//! [`KeyedLocks::get`] drops the entry for a key when the map holds the only
//! reference to it ([`Arc::strong_count`] of 1), and it does so while holding the
//! map lock. That is sufficient, because a handle to a key's mutex can *only* be
//! obtained from [`get`](KeyedLocks::get), which takes the same map lock: if the
//! count is 1 with the map lock held, no handle exists anywhere in the process,
//! so no task is inside the critical section for that key and none can be about
//! to enter it. A later caller simply creates a fresh mutex and has no one to
//! exclude against — which is exactly the state the key was in before its first
//! use.
//!
//! Eviction is a sweep on insert rather than a hook on drop so that the type
//! stays a plain `Arc` handout: callers keep using `.lock()` / `.lock_owned()`
//! on a `tokio::sync::Mutex` and need no guard wrapper.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Mutex as AsyncMutex;

/// A set of keyed async mutexes that forgets keys nobody is using.
///
/// Operations sharing a key never run concurrently; keys with no live holder are
/// dropped, so the map tracks the *in-flight* working set rather than every key
/// ever seen. See the module docs for the eviction-safety argument.
#[derive(Debug, Default)]
pub struct KeyedLocks {
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl KeyedLocks {
    /// An empty set of locks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The mutex for `key`, creating it on first use.
    ///
    /// Also drops every entry the map is the sole owner of — see the module
    /// docs for why that cannot race a caller into the critical section.
    #[must_use]
    pub fn get(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut map = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|k, lock| k == key || Arc::strong_count(lock) > 1);
        Arc::clone(map.entry(key.to_owned()).or_default())
    }

    /// How many keys the map is currently tracking. Test/diagnostic only.
    #[must_use]
    pub fn len(&self) -> usize {
        self.locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether the map is tracking no keys at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_hands_back_the_same_mutex_while_a_holder_is_alive() {
        let locks = KeyedLocks::new();
        let a = locks.get("k");
        let b = locks.get("k");
        assert!(Arc::ptr_eq(&a, &b), "a live key must keep its identity");
    }

    #[test]
    fn distinct_keys_get_distinct_mutexes() {
        let locks = KeyedLocks::new();
        let a = locks.get("a");
        let b = locks.get("b");
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn released_keys_are_evicted_so_the_map_does_not_grow_without_bound() {
        let locks = KeyedLocks::new();
        for i in 0..1000 {
            // Each handle is dropped at the end of the iteration, exactly as a
            // completed transcode/extraction releases its lock.
            let _guard = locks.get(&format!("/cache/session-{i}/playlist.m3u8"));
        }
        assert_eq!(
            locks.len(),
            1,
            "only the most recent key may remain; anything more is the unbounded growth this type exists to prevent"
        );
    }

    #[test]
    fn a_held_key_survives_other_keys_churning_past_it() {
        let locks = KeyedLocks::new();
        let held = locks.get("held");
        for i in 0..100 {
            let _t = locks.get(&format!("transient-{i}"));
        }
        assert!(
            Arc::ptr_eq(&held, &locks.get("held")),
            "eviction must never steal a key that still has a live holder"
        );
    }

    #[tokio::test]
    async fn the_same_key_still_serialises_two_tasks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let locks = Arc::new(KeyedLocks::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (locks, inside, max) = (Arc::clone(&locks), Arc::clone(&inside), Arc::clone(&max));
            tasks.push(tokio::spawn(async move {
                for _ in 0..25 {
                    let lock = locks.get("shared");
                    let _guard = lock.lock().await;
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for t in tasks {
            t.await.expect("task");
        }
        assert_eq!(
            max.load(Ordering::SeqCst),
            1,
            "eviction must not let two tasks into the same key's critical section"
        );
    }
}
