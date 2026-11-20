//! [`ChangeWatch`] — tells a run whether the key it works on changed while it
//! ran.
//!
//! A run reads what a key holds, works for a while, then saves a result made
//! from what it read. A change meanwhile must not be undone by that save: a
//! changed video file's prune deletes what was extracted from the old file,
//! and an extraction of the old file still running must not save its result
//! afterwards (gl-1694). The run [`watch`](ChangeWatch::watch)es the key
//! before it reads, the change goes through [`change`](ChangeWatch::change),
//! and the run saves through [`commit`](ChangeWatch::commit), which drops the
//! save when the key changed. One async lock is held across a change's write
//! and a commit's check and save, so neither lands inside the other.
//!
//! Only keys a run watches are remembered, as [`crate::keyed_locks`] keeps
//! only the locks in use: a change to a key no run watches records nothing,
//! and a key no run watches any more is dropped at the next watch. The media
//! segment manager's revisions (gl-1316) work the same way, for its own
//! writes.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Weak};

use tokio::sync::Mutex;

/// Each watched key's runs (the `Arc` behind the `Weak` is shared by their
/// [`Watch`]es) and how many changes it has had while watched.
type Keys<K> = HashMap<K, (Weak<()>, u64)>;

/// Which watched keys changed while a run worked on them (see the module
/// docs).
#[derive(Debug)]
pub struct ChangeWatch<K> {
    keys: Mutex<Keys<K>>,
}

impl<K> Default for ChangeWatch<K> {
    fn default() -> Self {
        Self {
            keys: Mutex::new(HashMap::new()),
        }
    }
}

/// A run's watch on one key, from [`ChangeWatch::watch`]; dropping it ends
/// the watch.
#[derive(Debug)]
pub struct Watch<K> {
    key: K,
    changes: u64,
    /// Keeps the key's entry alive; `None` for a [`Watch::detached`] watch.
    run: Option<Arc<()>>,
}

impl<K> Watch<K> {
    /// A watch no [`ChangeWatch`] tracks, for a store with no change to watch
    /// for (a test fake, or a trait's default). A [`ChangeWatch`] reads it as
    /// changed.
    #[must_use]
    pub fn detached(key: K) -> Self {
        Self {
            key,
            changes: 0,
            run: None,
        }
    }

    /// The watched key.
    #[must_use]
    pub fn key(&self) -> &K {
        &self.key
    }
}

impl<K: Eq + Hash + Clone> ChangeWatch<K> {
    /// Starts a run's watch on `key`, before the run reads what it holds.
    pub async fn watch(&self, key: K) -> Watch<K> {
        let mut keys = self.keys.lock().await;
        // The keys no run watches any more go, as `KeyedLocks::get` sweeps.
        keys.retain(|watched, (runs, _)| *watched == key || runs.strong_count() > 0);
        let (runs, changes) = keys.entry(key.clone()).or_default();
        let run = runs.upgrade().unwrap_or_else(|| {
            let run = Arc::new(());
            *runs = Arc::downgrade(&run);
            run
        });
        Watch {
            key,
            changes: *changes,
            run: Some(run),
        }
    }

    /// Marks `key` changed for every run watching it, then runs `write`, with
    /// no commit in between. `write` must not call into this `ChangeWatch`:
    /// its lock is held across it.
    pub async fn change<T>(&self, key: &K, write: impl Future<Output = T>) -> T {
        let mut keys = self.keys.lock().await;
        if let Some((_, changes)) = keys.get_mut(key) {
            *changes = changes.wrapping_add(1);
        }
        write.await
    }

    /// Runs `save` unless the watched key changed since `watch` began; `None`
    /// when it did. `save` must not call into this `ChangeWatch`: its lock is
    /// held across it.
    pub async fn commit<T>(&self, watch: Watch<K>, save: impl Future<Output = T>) -> Option<T> {
        let keys = self.keys.lock().await;
        if Self::moved(&keys, &watch) {
            return None;
        }
        let saved = save.await;
        drop(keys);
        Some(saved)
    }

    /// Whether the watched key changed since `watch` began.
    pub async fn changed(&self, watch: &Watch<K>) -> bool {
        Self::moved(&*self.keys.lock().await, watch)
    }

    fn moved(keys: &Keys<K>, watch: &Watch<K>) -> bool {
        watch.run.is_none()
            || keys
                .get(&watch.key)
                .is_none_or(|(_, changes)| *changes != watch.changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_commit_saves_unless_its_key_changed_since_the_watch() {
        let keys = ChangeWatch::default();
        let quiet = keys.watch("a").await;
        assert_eq!(keys.commit(quiet, async { 1 }).await, Some(1));

        let early = keys.watch("a").await;
        let other = keys.watch("b").await;
        keys.change(&"a", async {}).await;
        let late = keys.watch("a").await;
        assert!(keys.changed(&early).await);
        assert!(
            !keys.changed(&other).await,
            "another key's change is not this one's"
        );
        assert_eq!(keys.commit(early, async { 2 }).await, None);
        assert_eq!(
            keys.commit(late, async { 3 }).await,
            Some(3),
            "a run watching after the change saves"
        );
        assert_eq!(keys.commit(other, async { 4 }).await, Some(4));
    }

    /// A change started while a commit saves waits for the save, so the save
    /// cannot land after the change's write.
    #[tokio::test]
    async fn a_change_waits_for_a_commit_saving() {
        let keys = Arc::new(ChangeWatch::default());
        let watch = keys.watch("a").await;
        let saving = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let commit = tokio::spawn({
            let (keys, saving, release) =
                (Arc::clone(&keys), Arc::clone(&saving), Arc::clone(&release));
            async move {
                let save = async {
                    saving.notify_one();
                    release.notified().await;
                    1
                };
                keys.commit(watch, save).await
            }
        });
        saving.notified().await;
        let change = tokio::spawn({
            let keys = Arc::clone(&keys);
            async move { keys.change(&"a", async { 2 }).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!change.is_finished(), "the change waits for the save");
        release.notify_one();
        assert_eq!(commit.await.unwrap(), Some(1));
        assert_eq!(change.await.unwrap(), 2);
    }

    #[tokio::test]
    async fn only_watched_keys_are_remembered() {
        let keys = ChangeWatch::default();
        keys.change(&"unwatched", async {}).await;
        for key in ["a", "b", "c"] {
            drop(keys.watch(key).await);
        }
        let held = keys.watch("d").await;
        assert_eq!(
            keys.keys.lock().await.len(),
            1,
            "only the watched key stays"
        );
        assert!(!keys.changed(&held).await);
    }

    #[tokio::test]
    async fn a_detached_watch_reads_as_changed() {
        let keys = ChangeWatch::default();
        let _watched = keys.watch("a").await;
        assert!(keys.changed(&Watch::detached("a")).await);
        assert_eq!(keys.commit(Watch::detached("a"), async { 1 }).await, None);
    }
}
