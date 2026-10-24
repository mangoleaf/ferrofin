//! The debounced `LibraryChanged` push — port of
//! `Emby.Server.Implementations/EntryPoints/LibraryChangedNotifier.cs`.
//!
//! Jellyfin's notifier subscribes to `ILibraryManager.ItemAdded` /
//! `ItemUpdated` / `ItemRemoved` and accumulates the changed items, keyed by
//! id. The first change of a batch starts a one-shot timer of
//! `LibraryUpdateDuration` seconds that later changes never extend (Jellyfin
//! 12, jellyfin/jellyfin#17762), and a batch that reaches `MaxBatchSize` is
//! sent at once. When the timer fires it folds the accumulated sets into a
//! [`LibraryUpdateInfo`] and pushes it so open clients refresh their views.
//!
//! Ferrofin published `LibraryChanged` at **scan end only**, which meant a
//! metadata edit or a delete through the API pushed nothing at all: a client
//! sitting on a library view never learned the item had changed. That gap was
//! measured on 2026-08-31 by `suite/parity/push.py`, which saw Jellyfin push a
//! `LibraryChanged` that Ferrofin did not (`j=1, h=0`).
//!
//! Rust shape: there are no C# events here, so the three hooks are direct calls
//! from `FerrofinLibraryManager`'s `create_items` / `update_items` /
//! `delete_item` — the same three write paths the C# events fire from.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::entities_media::LibraryUpdateInfo;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::events::{EventManager, LibraryChangeAudience};
use uuid::Uuid;

/// Jellyfin's `ServerConfiguration.LibraryUpdateDuration` default, in seconds
/// (`configuration_manager.rs` seeds the same 30).
pub const DEFAULT_LIBRARY_UPDATE_DURATION_SECS: u64 = 30;

/// `LibraryChangedNotifier.MaxBatchSize`: a batch holding this many changed
/// items (added + updated + removed) is pushed at once, without waiting out
/// its window.
const MAX_BATCH_SIZE: usize = 2000;

/// One changed item, reduced at record time to just what the flush needs.
///
/// The top parent is captured here rather than looked up at flush time: the
/// row is already in hand, and by the time the timer fires a removed item is
/// gone from the database and could not be resolved at all.
#[derive(Debug, Clone)]
struct Changed {
    /// The item id in Jellyfin's `ToString("N")` spelling — jellyfin-web
    /// compares these against card `data-id` strings.
    id: String,
    /// The owning library, for per-user visibility filtering.
    top_parent: Option<String>,
    /// The parent folder, recorded into the batch's folder buckets.
    parent: Option<String>,
}

/// Which `ILibraryManager` event a change came from.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Added,
    Updated,
    Removed,
}

/// One kind of change, keyed by item id (`Dictionary<Guid, BaseItem>`): a
/// repeat replaces its entry in place, so a batch grows with the items it
/// names, not with the saves.
#[derive(Debug, Default)]
struct Bucket {
    items: Vec<Changed>,
    index: HashMap<String, usize>,
}

impl Bucket {
    /// `itemsList[item.Id] = item`.
    fn insert(&mut self, changed: Changed) {
        if let Some(&at) = self.index.get(&changed.id) {
            self.items[at] = changed;
        } else {
            self.index.insert(changed.id.clone(), self.items.len());
            self.items.push(changed);
        }
    }
}

/// What has changed since the last flush.
#[derive(Debug, Default)]
struct Pending {
    added: Bucket,
    updated: Bucket,
    removed: Bucket,
    /// `_foldersAddedTo` / `_foldersRemovedFrom`: every parent folder an added
    /// or removed item was recorded under, keyed by folder id. Each entry
    /// carries the item's top parent, for the per-user filter.
    folders_added_to: Bucket,
    folders_removed_from: Bucket,
    /// The open window's id, `None` while no batch is open
    /// (`_libraryUpdateTimer is null`). A timer task flushes only the window
    /// it was started for.
    window: Option<u64>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.added.items.is_empty()
            && self.updated.items.is_empty()
            && self.removed.items.is_empty()
    }

    fn len(&self) -> usize {
        self.added.items.len() + self.updated.items.len() + self.removed.items.len()
    }

    /// `OnLibraryChange`'s inserts: the item into its bucket and, for an
    /// addition or removal, its parent into that kind's folder bucket.
    fn insert(&mut self, kind: Kind, changed: Changed) {
        let (items, folders) = match kind {
            Kind::Added => (&mut self.added, Some(&mut self.folders_added_to)),
            Kind::Updated => (&mut self.updated, None),
            Kind::Removed => (&mut self.removed, Some(&mut self.folders_removed_from)),
        };
        if let (Some(folders), Some(parent)) = (folders, &changed.parent) {
            folders.insert(Changed {
                id: parent.clone(),
                top_parent: changed.top_parent.clone(),
                parent: None,
            });
        }
        items.insert(changed);
    }
}

/// The in-process event every debounced change set is published as, its
/// payload the unfiltered `LibraryUpdateInfo`; never forwarded to clients
/// (`LibraryChanged` is, filtered per user).
pub const ITEMS_CHANGED: &str = "ItemsChanged";

/// Accumulates library changes and pushes a folded `LibraryChanged` at the
/// end of each batch's window.
pub struct LibraryChangedNotifier {
    events: Arc<dyn EventManager>,
    /// The window length without a configuration to read, or when reading
    /// it fails.
    debounce: Duration,
    /// Where `LibraryUpdateDuration` is read as each window opens. `None`
    /// (unit tests) uses `debounce`.
    config: Option<Arc<dyn ServerConfigurationManager>>,
    pending: Mutex<Pending>,
    /// The id the next window gets ([`Pending::window`]).
    next_window: AtomicU64,
    /// The per-user fan-out. Set by the composition root after the library
    /// manager it reads from exists. `None` — unit tests, and any moment
    /// before wiring completes — falls back to publishing one payload on the
    /// event bus, which nothing forwards to clients.
    audience: std::sync::OnceLock<Arc<dyn LibraryChangeAudience>>,
}

impl LibraryChangedNotifier {
    /// Creates a notifier that publishes through `events`, each batch
    /// `debounce` after its first change unless [`Self::with_config`] wires
    /// the configured duration.
    #[must_use]
    pub fn new(events: Arc<dyn EventManager>, debounce: Duration) -> Self {
        Self {
            events,
            debounce,
            config: None,
            pending: Mutex::new(Pending::default()),
            next_window: AtomicU64::new(0),
            audience: std::sync::OnceLock::new(),
        }
    }

    /// Reads each window's length from the server configuration's
    /// `LibraryUpdateDuration` as the window opens, so a change to it applies
    /// to the next batch without a restart.
    #[must_use]
    pub fn with_config(mut self, config: Arc<dyn ServerConfigurationManager>) -> Self {
        self.config = Some(config);
        self
    }

    /// Wires the per-user fan-out. Called once by the composition root, after
    /// the library manager this notifier is attached to has been constructed.
    ///
    /// Later calls are ignored: the audience is a wiring decision, and letting
    /// it be swapped at runtime would mean a push could be addressed by one
    /// audience and delivered by another.
    pub fn set_audience(&self, audience: Arc<dyn LibraryChangeAudience>) {
        let _ = self.audience.set(audience);
    }

    /// `FilterItem`: which changes are worth announcing at all.
    ///
    /// A non-folder with no path has no `HasPathProtocol` and is skipped; a
    /// by-name item (genre, studio, person, year) is skipped unless it is a
    /// `MusicArtist`, which Jellyfin exempts because artists are browsable
    /// library content rather than a facet.
    fn filter_item(item: &BaseItemEntity) -> bool {
        if !item.is_folder && item.path.as_deref().unwrap_or_default().is_empty() {
            return false;
        }
        let Some(kind) = crate::item_type_lookup::kind_from_type_name(&item.type_) else {
            return true;
        };
        !crate::kinds::is_item_by_name(kind)
            || kind == ferrofin_model::data::BaseItemKind::MusicArtist
    }

    fn changed_of(item: &BaseItemEntity) -> Option<Changed> {
        let n = |s: &str| Uuid::parse_str(s).ok().map(|id| id.simple().to_string());
        Some(Changed {
            id: n(&item.id)?,
            top_parent: item.top_parent_id.as_deref().and_then(n),
            parent: item.parent_id.as_deref().and_then(n),
        })
    }

    /// Records created items (`ItemAdded`).
    pub fn record_added(self: &Arc<Self>, items: &[BaseItemEntity]) {
        self.record(items, Kind::Added);
    }

    /// Records saved items (`ItemUpdated`).
    pub fn record_updated(self: &Arc<Self>, items: &[BaseItemEntity]) {
        self.record(items, Kind::Updated);
    }

    /// Records deleted items (`ItemRemoved`).
    ///
    /// Takes the rows rather than bare ids because the flush needs the owning
    /// library and parent folder, which are unresolvable once the rows are gone.
    pub fn record_removed(self: &Arc<Self>, items: &[BaseItemEntity]) {
        self.record(items, Kind::Removed);
    }

    fn record(self: &Arc<Self>, items: &[BaseItemEntity], kind: Kind) {
        let changed: Vec<Changed> = items
            .iter()
            .filter(|i| Self::filter_item(i))
            .filter_map(Self::changed_of)
            .collect();
        self.add(changed, kind);
    }

    /// `OnLibraryChange`, per item: adds it to its bucket, opens a window of
    /// `LibraryUpdateDuration` at a batch's first change (never extending
    /// one already open: a window that moved with every change would keep a
    /// library scan's batch open for the whole scan), and sends the batch at
    /// once when it reaches [`MAX_BATCH_SIZE`], so one large call goes out as
    /// several full batches.
    fn add(self: &Arc<Self>, changed: Vec<Changed>, kind: Kind) {
        let mut opened = Vec::new();
        let mut full = Vec::new();
        {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            for change in changed {
                if pending.window.is_none() {
                    let window = self.next_window.fetch_add(1, Ordering::SeqCst);
                    pending.window = Some(window);
                    opened.push(window);
                }
                pending.insert(kind, change);
                if pending.len() >= MAX_BATCH_SIZE {
                    // `_libraryUpdateTimer.Change(TimeSpan.Zero, …)`: the batch
                    // is taken now, so its window's timer finds it closed.
                    full.push(std::mem::take(&mut *pending));
                }
            }
        }
        for window in opened {
            self.close_window_after(window);
        }
        if !full.is_empty() {
            let this = Arc::clone(self);
            tokio::spawn(async move {
                for batch in full {
                    this.publish(batch).await;
                }
            });
        }
    }

    /// Fires `window`'s timer: the batch is sent unless that window has
    /// already closed.
    fn close_window_after(self: &Arc<Self>, window: u64) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(this.window_length().await).await;
            if let Some(pending) = this.take(Some(window)) {
                this.publish(pending).await;
            }
        });
    }

    /// The length of a window opening now: `TimeSpan.FromSeconds(
    /// LibraryUpdateDuration)`, read when the window opens, with zero sending
    /// the batch at once. A negative value counts as zero (upstream's `Timer`
    /// rejects it).
    async fn window_length(&self) -> Duration {
        let Some(config) = &self.config else {
            return self.debounce;
        };
        match config.configuration().await {
            Ok(c) => Duration::from_secs(u64::try_from(c.library_update_duration).unwrap_or(0)),
            Err(err) => {
                tracing::warn!(%err, "failed to read LibraryUpdateDuration; using the default");
                self.debounce
            }
        }
    }

    /// Takes the batch and closes its window, when `window` is the one open
    /// (any window for `None`); `None` when there is nothing to take.
    fn take(&self, window: Option<u64>) -> Option<Pending> {
        let mut guard = self.pending.lock().ok()?;
        if window.is_some_and(|window| guard.window != Some(window)) {
            return None;
        }
        let pending = std::mem::take(&mut *guard);
        (!pending.is_empty()).then_some(pending)
    }

    /// Folds the accumulated changes and publishes them now
    /// (`LibraryUpdateTimerCallback`), closing the open window.
    ///
    /// Public so a caller that needs the push *now* — a test, or a shutdown
    /// path that must not lose a pending announcement — can force it.
    pub async fn flush(&self) {
        if let Some(pending) = self.take(None) {
            self.publish(pending).await;
        }
    }

    /// Folds one taken batch and publishes it, per user when an audience is
    /// wired.
    async fn publish(&self, pending: Pending) {
        let folded = fold(&pending);
        if folded.info.is_empty {
            return;
        }
        // The whole change set for in-process consumers (the server's own
        // `ILibraryManager.ItemAdded`/`ItemUpdated`/`ItemRemoved` subscribers,
        // such as the intro skipper), before any per-user filtering.
        let payload = serde_json::to_string(&folded.info).ok();
        if let Some(payload) = &payload {
            let _ = self.events.publish(ITEMS_CHANGED, payload).await;
        }
        let Some(audience) = self.audience.get() else {
            // No fan-out wired (unit tests): one unfiltered payload on the
            // bus, which nothing forwards to clients.
            if let Some(payload) = &payload {
                let _ = self.events.publish("LibraryChanged", payload).await;
            }
            return;
        };
        let Ok(user_ids) = audience.active_user_ids().await else {
            return;
        };
        for user_id in user_ids {
            let Ok(visible) = audience.visible_library_ids(user_id).await else {
                continue;
            };
            // An item names its library through its `TopParentId`: the
            // library, or one of its locations' folders.
            let tops = audience
                .library_top_parents(&visible)
                .await
                .unwrap_or_else(|_| visible.clone());
            let n = |ids: &[Uuid]| -> HashSet<String> {
                ids.iter().map(|id| id.simple().to_string()).collect()
            };
            let mut lists = folded.for_libraries(&n(&tops));
            // Whether any folder gained or lost a child, which is what names
            // the user's libraries in `CollectionFolders`. Upstream reads the
            // folder lists before they are translated for the user; this keeps
            // the cut by library (and, as before, the removed-from folders
            // uncut) but not the item-visibility filter below.
            let folders_moved =
                !lists.folders_added_to.is_empty() || !folded.lists.folders_removed_from.is_empty();
            // `TranslatePhysicalItemToUserLibrary`: within those libraries an
            // item is announced only when the user may see it on its own
            // (`IsVisibleStandalone`): another user's private playlist, or an
            // item above the user's parental rating, is not. A folder deleted
            // in the same batch has no row left and is not named either; its
            // id is in the unfiltered removals.
            let candidates = lists.announced_ids();
            let seen = match audience.visible_item_ids(user_id, &candidates).await {
                Ok(seen) => n(&seen),
                Err(err) => {
                    // Fail closed: the user is still told about removals.
                    tracing::warn!(
                        %err,
                        %user_id,
                        "failed to check which changed items a user can see; \
                         announcing only removals"
                    );
                    HashSet::new()
                }
            };
            lists.retain_visible(&seen);
            let info = build(&lists, Some((&n(&visible), folders_moved)));
            // `if (info.IsEmpty) continue;` — a user who can see none of the
            // changed libraries is told nothing at all, rather than being
            // handed an empty envelope that makes their client re-fetch.
            if info.is_empty {
                continue;
            }
            let Ok(payload) = serde_json::to_string(&info) else {
                continue;
            };
            if let Err(err) = audience.deliver(user_id, &payload).await {
                tracing::debug!(%err, %user_id, "failed to push LibraryChanged");
            }
        }
    }
}

/// One batch's deduped lists.
#[derive(Default)]
struct Lists {
    added: Vec<Changed>,
    updated: Vec<Changed>,
    removed: Vec<Changed>,
    folders_added_to: Vec<Changed>,
    folders_removed_from: Vec<Changed>,
}

impl Lists {
    /// The distinct ids a user is told about only when they may see them:
    /// everything but the removals.
    fn announced_ids(&self) -> Vec<Uuid> {
        let mut seen = HashSet::new();
        self.added
            .iter()
            .chain(&self.updated)
            .chain(&self.folders_added_to)
            .chain(&self.folders_removed_from)
            .filter(|c| seen.insert(c.id.as_str()))
            .filter_map(|c| Uuid::parse_str(&c.id).ok())
            .collect()
    }

    /// Keeps, in every list but the removals, only the ids in `visible`.
    fn retain_visible(&mut self, visible: &HashSet<String>) {
        for list in [
            &mut self.added,
            &mut self.updated,
            &mut self.folders_added_to,
            &mut self.folders_removed_from,
        ] {
            list.retain(|c| visible.contains(&c.id));
        }
    }
}

/// The deduped change set, kept alongside the payload it folds into.
///
/// The per-user filter needs each item's owning library, which the wire
/// [`LibraryUpdateInfo`] does not carry — it is a flat list of ids. So the
/// folded lists are retained and [`Folded::for_libraries`] re-derives one
/// user's payload from them.
struct Folded {
    lists: Lists,
    /// The unfiltered payload, used when no per-user audience is wired.
    info: LibraryUpdateInfo,
}

impl Folded {
    /// One user's lists for `GetLibraryUpdateInfo`, cut to the libraries they
    /// can see; [`Lists::retain_visible`] then applies the item's own
    /// visibility.
    ///
    /// Removals are NOT filtered. C# passes `includeIfNotFound: true` for them
    /// alone, because the row is already gone and `IsVisibleStandalone` has
    /// nothing left to test — telling a client to drop an id it never had is
    /// harmless, whereas withholding it leaves a deleted item on screen. The
    /// folders an item was removed from still exist, and are filtered like the
    /// rest (`TranslatePhysicalItemToUserLibrary(i, user)`).
    ///
    /// `tops` are the `TopParentId`s the user's libraries' items carry (each
    /// library and its locations' folders).
    fn for_libraries(&self, tops: &HashSet<String>) -> Lists {
        let keep = |v: &[Changed]| -> Vec<Changed> {
            v.iter()
                .filter(|c| c.top_parent.as_ref().is_some_and(|t| tops.contains(t)))
                .cloned()
                .collect()
        };
        Lists {
            added: keep(&self.lists.added),
            updated: keep(&self.lists.updated),
            removed: self.lists.removed.clone(),
            folders_added_to: keep(&self.lists.folders_added_to),
            folders_removed_from: keep(&self.lists.folders_removed_from),
        }
    }
}

/// Builds the wire payload from one batch's deduped lists.
///
/// `recipient` is, when there is one, the set of libraries they can see and
/// whether any folder gained or lost a child among the changes they were
/// considered for. It is what `CollectionFolders` is derived from — see below.
fn build(lists: &Lists, recipient: Option<(&HashSet<String>, bool)>) -> LibraryUpdateInfo {
    let ids = |v: &[Changed]| -> Vec<String> { v.iter().map(|c| c.id.clone()).collect() };
    let folders_added_to = ids(&lists.folders_added_to);
    let folders_removed_from = ids(&lists.folders_removed_from);

    // `GetTopParentIds(newAndRemoved, allUserRootChildren)`, and it is NOT the
    // changed items' own libraries. C# passes it `foldersAddedTo +
    // foldersRemovedFrom` and, for each one, adds EVERY library the user can
    // see, then distincts. So the field means "membership moved, re-read your
    // libraries" -- all of them -- and a plain metadata edit, which moves
    // nothing between folders, names NONE. Deriving it from each item's top
    // parent instead made an edit name the edited item's library, which
    // suite/parity/push.py caught as a diff at `Data.CollectionFolders[]`.
    //
    // ponytail: order is the visible-id order, not C#'s name-sorted root
    // children. Only observable when folders actually change; sort by library
    // name here if a client is ever seen to care.
    let collection_folders: Vec<String> = match recipient {
        Some((libs, true)) => {
            let mut v: Vec<String> = libs.iter().cloned().collect();
            v.sort();
            v
        }
        Some((_, false)) => Vec::new(),
        // No recipient to scope to (no audience wired): fall back to the
        // top parents of the changed items (their locations' folders, or on
        // older rows their libraries), which is the most a broadcast can
        // honestly say without the library list.
        None => {
            let mut seen = HashSet::new();
            lists
                .added
                .iter()
                .chain(&lists.removed)
                .chain(&lists.updated)
                .filter_map(|c| c.top_parent.clone())
                .filter(|t| seen.insert(t.clone()))
                .collect()
        }
    };

    let mut info = LibraryUpdateInfo {
        folders_added_to,
        folders_removed_from,
        items_added: ids(&lists.added),
        items_removed: ids(&lists.removed),
        items_updated: ids(&lists.updated),
        collection_folders,
        ..LibraryUpdateInfo::default()
    };
    info.is_empty = info.compute_is_empty();
    info
}

/// Dedupes the accumulated buckets (`LibraryUpdateTimerCallback`).
///
/// `ItemsUpdated` excludes anything also in `ItemsAdded` (`_itemsUpdated.Where(i
/// => !_itemsAdded.Contains(i))`): an item created and then saved in the same
/// window is an addition, and reporting it twice makes a client fetch it twice.
fn fold(pending: &Pending) -> Folded {
    let updated: Vec<Changed> = pending
        .updated
        .items
        .iter()
        .filter(|c| !pending.added.index.contains_key(&c.id))
        .cloned()
        .collect();
    let lists = Lists {
        added: pending.added.items.clone(),
        updated,
        removed: pending.removed.items.clone(),
        folders_added_to: pending.folders_added_to.items.clone(),
        folders_removed_from: pending.folders_removed_from.items.clone(),
    };
    let info = build(&lists, None);
    Folded { lists, info }
}

impl LibraryChangedNotifier {
    /// Records items deleted from `library` that are known only by id: a
    /// library scan's prune and the old ids of the items it moved, whose rows
    /// are gone by the time the scan announces them. Each is filed under the
    /// library, as its top parent and as the folder it was removed from
    /// (upstream names each item's own parent). With no row to read,
    /// `FilterItem` is not applied: the prune removes library items, which it
    /// admits.
    pub fn record_removed_from_library(self: &Arc<Self>, library: Uuid, ids: &[Uuid]) {
        let library = library.simple().to_string();
        let changed = ids
            .iter()
            .map(|id| Changed {
                id: id.simple().to_string(),
                top_parent: Some(library.clone()),
                parent: Some(library.clone()),
            })
            .collect();
        self.add(changed, Kind::Removed);
    }

    /// Records a folder deletion that cascaded to its children.
    ///
    /// `delete_item` loads the folder row and then resolves its children as
    /// bare ids, so the children have no rows of their own to filter or read a
    /// top parent from. They inherit both from `root`: a child of a deleted
    /// folder is in the same library, and its parent is the folder itself.
    /// `FilterItem` is not re-applied per child — the children of a deletable
    /// library folder are library items with paths, which is exactly what it
    /// admits.
    pub fn record_removed_subtree(self: &Arc<Self>, root: &BaseItemEntity, children: &[Uuid]) {
        let root_changed = Self::filter_item(root)
            .then(|| Self::changed_of(root))
            .flatten();
        let root_n = Uuid::parse_str(&root.id)
            .ok()
            .map(|i| i.simple().to_string());
        let top = root_changed
            .as_ref()
            .and_then(|c| c.top_parent.clone())
            .or_else(|| root_n.clone());
        let mut all: Vec<Changed> = root_changed.into_iter().collect();
        all.extend(children.iter().map(|id| Changed {
            id: id.simple().to_string(),
            top_parent: top.clone(),
            parent: root_n.clone(),
        }));
        self.add(all, Kind::Removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_traits::error::ServiceError;

    /// Captures every published payload so a test can assert on the push
    /// itself, not on a proxy for it.
    #[derive(Default)]
    struct RecordingEvents {
        published: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl EventManager for RecordingEvents {
        async fn publish(&self, event_type: &str, payload: &str) -> Result<(), ServiceError> {
            self.published
                .lock()
                .expect("lock")
                .push((event_type.to_owned(), payload.to_owned()));
            Ok(())
        }
    }

    impl RecordingEvents {
        fn library_updates(&self) -> Vec<LibraryUpdateInfo> {
            self.published
                .lock()
                .expect("lock")
                .iter()
                .filter(|(t, _)| t == "LibraryChanged")
                .map(|(_, p)| serde_json::from_str(p).expect("payload parses"))
                .collect()
        }
    }

    fn notifier(debounce: Duration) -> (Arc<LibraryChangedNotifier>, Arc<RecordingEvents>) {
        let events = Arc::new(RecordingEvents::default());
        let n = Arc::new(LibraryChangedNotifier::new(
            Arc::clone(&events) as Arc<dyn EventManager>,
            debounce,
        ));
        (n, events)
    }

    /// A configuration whose `LibraryUpdateDuration` a test can change.
    struct LiveConfig(Mutex<ferrofin_model::configuration::ServerConfiguration>);

    impl LiveConfig {
        fn set(&self, seconds: i32) {
            self.0.lock().expect("lock").library_update_duration = seconds;
        }
    }

    #[async_trait::async_trait]
    impl ServerConfigurationManager for LiveConfig {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unreachable!("not used by the notifier")
        }
        async fn configuration(
            &self,
        ) -> Result<Arc<ferrofin_model::configuration::ServerConfiguration>, ServiceError> {
            Ok(Arc::new(self.0.lock().expect("lock").clone()))
        }
        async fn update_configuration(
            &self,
            configuration: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ServiceError> {
            *self.0.lock().expect("lock") = configuration.clone();
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn movie(id: Uuid, parent: Uuid, library: Uuid) -> BaseItemEntity {
        BaseItemEntity {
            id: id.to_string(),
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            path: Some("/media/a.mkv".to_owned()),
            parent_id: Some(parent.to_string()),
            top_parent_id: Some(library.to_string()),
            is_folder: false,
            ..BaseItemEntity::default()
        }
    }

    #[test]
    fn a_pathless_non_folder_is_not_announced() {
        // `FilterItem`: no path means no `HasPathProtocol`.
        let mut item = movie(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        item.path = None;
        assert!(!LibraryChangedNotifier::filter_item(&item));
    }

    #[test]
    fn a_by_name_item_is_not_announced_but_a_music_artist_is() {
        let mut genre = movie(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        genre.type_ = "MediaBrowser.Controller.Entities.Genre".to_owned();
        assert!(!LibraryChangedNotifier::filter_item(&genre));

        let mut artist = genre.clone();
        artist.type_ = "MediaBrowser.Controller.Entities.Audio.MusicArtist".to_owned();
        assert!(
            LibraryChangedNotifier::filter_item(&artist),
            "C# exempts MusicArtist from the IItemByName skip"
        );
    }

    /// Jellyfin 12's window (jellyfin/jellyfin#17762): it runs from the first
    /// change of a batch and is never extended, so a steady stream of changes
    /// still pushes every `LibraryUpdateDuration`, and the next change after a
    /// push opens a new window.
    #[tokio::test(start_paused = true)]
    async fn the_window_runs_from_the_first_change_and_is_never_extended() {
        let (n, events) = notifier(Duration::from_secs(30));
        let library = Uuid::new_v4();
        let parent = Uuid::new_v4();

        // Three writes 10 s apart, all inside the first change's window.
        for _ in 0..3 {
            n.record_added(&[movie(Uuid::new_v4(), parent, library)]);
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 1, "one push, 30 s after the first change");
        assert_eq!(pushed[0].items_added.len(), 3);
        assert_eq!(
            pushed[0].collection_folders,
            vec![library.simple().to_string()]
        );
        assert!(!pushed[0].is_empty);

        // A later change opens a new window of its own.
        n.record_added(&[movie(Uuid::new_v4(), parent, library)]);
        tokio::time::sleep(Duration::from_secs(29)).await;
        assert_eq!(events.library_updates().len(), 1);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(events.library_updates().len(), 2);
    }

    /// The buckets are keyed by item id, so a repeat replaces its entry
    /// instead of growing the batch, and a batch that reaches
    /// `MaxBatchSize` (2000) is pushed at once, without waiting out its
    /// window.
    #[tokio::test(start_paused = true)]
    async fn a_batch_is_keyed_by_item_and_flushed_once_it_reaches_the_cap() {
        let (n, events) = notifier(Duration::from_secs(30));
        let library = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let same = movie(Uuid::new_v4(), parent, library);
        for _ in 0..MAX_BATCH_SIZE {
            n.record_updated(std::slice::from_ref(&same));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            events.library_updates().is_empty(),
            "one item saved 2000 times is one entry, not a full batch"
        );
        tokio::time::sleep(Duration::from_secs(30)).await;
        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].items_updated.len(), 1);

        let items: Vec<BaseItemEntity> = (0..MAX_BATCH_SIZE)
            .map(|_| movie(Uuid::new_v4(), parent, library))
            .collect();
        n.record_added(&items[..MAX_BATCH_SIZE - 1]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(events.library_updates().len(), 1, "one short of the cap");
        n.record_added(&items[MAX_BATCH_SIZE - 1..]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 2, "the cap pushes without waiting");
        assert_eq!(pushed[1].items_added.len(), MAX_BATCH_SIZE);

        // The capped batch's window ends with nothing left to push.
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert_eq!(events.library_updates().len(), 2);

        // One call over twice the cap goes out as two full batches at once,
        // and the remainder waits out a window of its own.
        let items: Vec<BaseItemEntity> = (0..=2 * MAX_BATCH_SIZE)
            .map(|_| movie(Uuid::new_v4(), parent, library))
            .collect();
        n.record_added(&items);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 4, "two full batches, without waiting");
        assert_eq!(pushed[2].items_added.len(), MAX_BATCH_SIZE);
        assert_eq!(pushed[3].items_added.len(), MAX_BATCH_SIZE);
        tokio::time::sleep(Duration::from_secs(30)).await;
        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 5);
        assert_eq!(pushed[4].items_added.len(), 1);
    }

    /// `LibraryUpdateDuration` is read as each window opens
    /// (`OnLibraryChange`), so a configuration change applies to the next
    /// batch without a restart; zero, or a negative value, sends it at once.
    #[tokio::test(start_paused = true)]
    async fn each_window_reads_the_configured_duration() {
        let config = Arc::new(LiveConfig(Mutex::new(
            ferrofin_model::configuration::ServerConfiguration {
                library_update_duration: 10,
                ..Default::default()
            },
        )));
        let events = Arc::new(RecordingEvents::default());
        let n = Arc::new(
            LibraryChangedNotifier::new(
                Arc::clone(&events) as Arc<dyn EventManager>,
                Duration::from_secs(30),
            )
            .with_config(Arc::clone(&config) as Arc<dyn ServerConfigurationManager>),
        );
        let (library, parent) = (Uuid::new_v4(), Uuid::new_v4());
        let add = || n.record_added(&[movie(Uuid::new_v4(), parent, library)]);

        add();
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert!(events.library_updates().is_empty());
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(events.library_updates().len(), 1, "a 10 s window");

        config.set(60);
        add();
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert_eq!(events.library_updates().len(), 1, "the new 60 s applies");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(events.library_updates().len(), 2);

        for seconds in [0, -5] {
            config.set(seconds);
            add();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            events.library_updates().len(),
            4,
            "zero and below send at once"
        );
    }

    /// A forced flush closes the window, so that window's timer, still
    /// asleep, must leave the next batch alone rather than cut it short.
    #[tokio::test(start_paused = true)]
    async fn a_closed_windows_timer_leaves_the_next_batch_alone() {
        let (n, events) = notifier(Duration::from_secs(30));
        let (library, parent) = (Uuid::new_v4(), Uuid::new_v4());
        n.record_added(&[movie(Uuid::new_v4(), parent, library)]);
        n.flush().await;
        assert_eq!(events.library_updates().len(), 1);

        tokio::time::sleep(Duration::from_secs(20)).await;
        n.record_added(&[movie(Uuid::new_v4(), parent, library)]);
        // The first window's timer wakes at 30 s, inside the second window.
        tokio::time::sleep(Duration::from_secs(29)).await;
        assert_eq!(
            events.library_updates().len(),
            1,
            "the closed window's timer sent the open batch"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(events.library_updates().len(), 2);
    }

    /// `_foldersRemovedFrom` is a dictionary of its own: every folder an item
    /// was removed from is reported, even when the item repeats under another.
    #[tokio::test(start_paused = true)]
    async fn every_folder_a_repeated_removal_named_is_reported() {
        let (n, events) = notifier(Duration::from_secs(30));
        let (library, season, series) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let episode = Uuid::new_v4();
        n.record_removed(&[movie(episode, season, library)]);
        n.record_removed(&[movie(episode, series, library)]);
        n.flush().await;

        let pushed = events.library_updates();
        assert_eq!(pushed[0].items_removed, vec![episode.simple().to_string()]);
        assert_eq!(
            pushed[0].folders_removed_from,
            vec![season.simple().to_string(), series.simple().to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_item_added_and_then_saved_is_reported_added_only() {
        let (n, events) = notifier(Duration::from_secs(30));
        let item = movie(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        n.record_added(std::slice::from_ref(&item));
        n.record_updated(std::slice::from_ref(&item));
        tokio::time::sleep(Duration::from_secs(31)).await;

        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].items_added.len(), 1);
        assert!(
            pushed[0].items_updated.is_empty(),
            "`_itemsUpdated.Where(i => !_itemsAdded.Contains(i))` — else the client fetches it twice"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_deleted_folder_announces_its_cascaded_children() {
        let (n, events) = notifier(Duration::from_secs(30));
        let library = Uuid::new_v4();
        let mut folder = movie(Uuid::new_v4(), library, library);
        folder.is_folder = true;
        folder.type_ = "MediaBrowser.Controller.Entities.Folder".to_owned();
        let children: Vec<Uuid> = (0..2).map(|_| Uuid::new_v4()).collect();

        n.record_removed_subtree(&folder, &children);
        tokio::time::sleep(Duration::from_secs(31)).await;

        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 1);
        assert_eq!(
            pushed[0].items_removed.len(),
            3,
            "the folder plus both children — the children's rows are already gone"
        );
        // Both parents, as C# records: the folder was removed FROM the library,
        // and each child was removed FROM the folder.
        assert_eq!(
            pushed[0].folders_removed_from,
            vec![
                library.simple().to_string(),
                Uuid::parse_str(&folder.id)
                    .expect("uuid")
                    .simple()
                    .to_string(),
            ]
        );
    }

    /// An audience with two users who can see different libraries.
    struct SplitAudience {
        /// user id -> the one library that user can see
        visible: Vec<(Uuid, Uuid)>,
        /// library -> the folder of its one location
        locations: Vec<(Uuid, Uuid)>,
        /// user id -> an item that user may not see on its own
        hidden: Vec<(Uuid, Uuid)>,
        /// Whether asking which items a user can see fails.
        visibility_fails: bool,
        delivered: Mutex<Vec<(Uuid, LibraryUpdateInfo)>>,
    }

    #[async_trait::async_trait]
    impl LibraryChangeAudience for SplitAudience {
        async fn active_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(self.visible.iter().map(|(u, _)| *u).collect())
        }
        async fn visible_library_ids(&self, user_id: Uuid) -> Result<Vec<Uuid>, ServiceError> {
            Ok(self
                .visible
                .iter()
                .filter(|(u, _)| *u == user_id)
                .map(|(_, l)| *l)
                .collect())
        }
        async fn library_top_parents(&self, libraries: &[Uuid]) -> Result<Vec<Uuid>, ServiceError> {
            let mut tops = libraries.to_vec();
            tops.extend(
                self.locations
                    .iter()
                    .filter(|(l, _)| libraries.contains(l))
                    .map(|(_, folder)| *folder),
            );
            Ok(tops)
        }
        async fn visible_item_ids(
            &self,
            user_id: Uuid,
            ids: &[Uuid],
        ) -> Result<Vec<Uuid>, ServiceError> {
            if self.visibility_fails {
                return Err(ServiceError::Backend("visibility unavailable".to_owned()));
            }
            Ok(ids
                .iter()
                .copied()
                .filter(|id| !self.hidden.contains(&(user_id, *id)))
                .collect())
        }
        async fn deliver(&self, user_id: Uuid, payload: &str) -> Result<(), ServiceError> {
            self.delivered.lock().expect("lock").push((
                user_id,
                serde_json::from_str(payload).expect("payload parses"),
            ));
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_is_never_told_about_a_library_they_cannot_see() {
        let (n, events) = notifier(Duration::from_secs(30));
        let (lib_a, lib_b) = (Uuid::new_v4(), Uuid::new_v4());
        let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
        let location_a = Uuid::new_v4();
        let audience = Arc::new(SplitAudience {
            visible: vec![(alice, lib_a), (bob, lib_b)],
            locations: vec![(lib_a, location_a), (lib_b, Uuid::new_v4())],
            hidden: Vec::new(),
            visibility_fails: false,
            delivered: Mutex::new(Vec::new()),
        });
        n.set_audience(Arc::clone(&audience) as Arc<dyn LibraryChangeAudience>);

        // One item added to library A only, under its location's folder.
        let item = movie(Uuid::new_v4(), Uuid::new_v4(), location_a);
        n.record_added(std::slice::from_ref(&item));
        tokio::time::sleep(Duration::from_secs(31)).await;

        let delivered = audience.delivered.lock().expect("lock");
        assert_eq!(
            delivered.len(),
            1,
            "Bob can only see library B, so his payload folds to empty and C# skips him \
             entirely — broadcasting would leak library A's item ids to him"
        );
        assert_eq!(delivered[0].0, alice);
        assert_eq!(
            delivered[0].1.items_added,
            vec![
                Uuid::parse_str(&item.id)
                    .expect("uuid")
                    .simple()
                    .to_string()
            ]
        );
        assert!(
            events.library_updates().is_empty(),
            "with an audience wired the push goes per-user, not to the broadcast bus"
        );
    }

    /// gl-580: within a library the user can see, an item is announced only
    /// when they may see it on its own (`IsVisibleStandalone`) — another
    /// user's private playlist, a folder they cannot open — while a removal
    /// reaches everyone (`includeIfNotFound: true`).
    #[tokio::test(start_paused = true)]
    async fn an_item_a_user_may_not_see_is_left_out_but_its_removal_is_not() {
        let (n, _events) = notifier(Duration::from_secs(30));
        let (lib, alice, bob) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (playlist, folder) = (Uuid::new_v4(), Uuid::new_v4());
        let audience = Arc::new(SplitAudience {
            visible: vec![(alice, lib), (bob, lib)],
            locations: Vec::new(),
            hidden: vec![(bob, playlist), (bob, folder)],
            visibility_fails: false,
            delivered: Mutex::new(Vec::new()),
        });
        n.set_audience(Arc::clone(&audience) as Arc<dyn LibraryChangeAudience>);

        let added = movie(Uuid::new_v4(), Uuid::new_v4(), lib);
        let mut private = movie(playlist, Uuid::new_v4(), lib);
        private.parent_id = None;
        let removed = movie(Uuid::new_v4(), folder, lib);
        n.record_added(std::slice::from_ref(&added));
        n.record_updated(std::slice::from_ref(&private));
        n.record_removed(std::slice::from_ref(&removed));
        tokio::time::sleep(Duration::from_secs(31)).await;

        let s = |id: &str| Uuid::parse_str(id).expect("uuid").simple().to_string();
        let delivered = audience.delivered.lock().expect("lock");
        let to = |user: Uuid| {
            delivered
                .iter()
                .find(|(u, _)| *u == user)
                .map(|(_, info)| info.clone())
                .expect("delivered")
        };
        let (alice_info, bob_info) = (to(alice), to(bob));
        assert_eq!(alice_info.items_updated, vec![s(&private.id)]);
        assert_eq!(
            alice_info.folders_removed_from,
            vec![s(&folder.to_string())]
        );
        assert_eq!(bob_info.items_added, vec![s(&added.id)]);
        assert!(bob_info.items_updated.is_empty(), "{bob_info:?}");
        assert!(bob_info.folders_removed_from.is_empty(), "{bob_info:?}");
        assert_eq!(bob_info.items_removed, vec![s(&removed.id)]);
    }

    /// `CollectionFolders` ("membership moved, re-read your libraries") comes
    /// from the folder lists before the item-visibility filter: a folder the
    /// user may not see still gained a child in one of their libraries.
    #[tokio::test(start_paused = true)]
    async fn a_hidden_folder_change_still_names_the_users_libraries() {
        let (n, _events) = notifier(Duration::from_secs(30));
        let (lib, bob) = (Uuid::new_v4(), Uuid::new_v4());
        let (hidden, folder) = (Uuid::new_v4(), Uuid::new_v4());
        let audience = Arc::new(SplitAudience {
            visible: vec![(bob, lib)],
            locations: Vec::new(),
            hidden: vec![(bob, hidden), (bob, folder)],
            visibility_fails: false,
            delivered: Mutex::new(Vec::new()),
        });
        n.set_audience(Arc::clone(&audience) as Arc<dyn LibraryChangeAudience>);

        n.record_added(&[movie(hidden, folder, lib)]);
        let mut edited = movie(Uuid::new_v4(), Uuid::new_v4(), lib);
        edited.parent_id = None;
        n.record_updated(std::slice::from_ref(&edited));
        tokio::time::sleep(Duration::from_secs(31)).await;

        let delivered = audience.delivered.lock().expect("lock");
        assert_eq!(delivered.len(), 1);
        let info = &delivered[0].1;
        assert!(info.items_added.is_empty() && info.folders_added_to.is_empty());
        assert_eq!(info.items_updated.len(), 1);
        assert_eq!(info.collection_folders, vec![lib.simple().to_string()]);
    }

    /// When the visibility check fails, the user is told only about removals.
    #[tokio::test(start_paused = true)]
    async fn a_failed_visibility_check_announces_only_removals() {
        let (n, _events) = notifier(Duration::from_secs(30));
        let (lib, alice) = (Uuid::new_v4(), Uuid::new_v4());
        let audience = Arc::new(SplitAudience {
            visible: vec![(alice, lib)],
            locations: Vec::new(),
            hidden: Vec::new(),
            visibility_fails: true,
            delivered: Mutex::new(Vec::new()),
        });
        n.set_audience(Arc::clone(&audience) as Arc<dyn LibraryChangeAudience>);

        n.record_added(&[movie(Uuid::new_v4(), Uuid::new_v4(), lib)]);
        let removed = movie(Uuid::new_v4(), Uuid::new_v4(), lib);
        n.record_removed(std::slice::from_ref(&removed));
        tokio::time::sleep(Duration::from_secs(31)).await;

        let delivered = audience.delivered.lock().expect("lock");
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].1.items_added.is_empty());
        assert!(delivered[0].1.folders_removed_from.is_empty());
        assert_eq!(delivered[0].1.items_removed.len(), 1);
    }

    /// In-process consumers get the whole change set, audience or not, as
    /// [`ITEMS_CHANGED`] — never as a client push.
    #[tokio::test(start_paused = true)]
    async fn in_process_consumers_get_every_change_unfiltered() {
        let (n, events) = notifier(Duration::from_secs(30));
        n.set_audience(Arc::new(SplitAudience {
            visible: Vec::new(),
            locations: Vec::new(),
            hidden: Vec::new(),
            visibility_fails: false,
            delivered: Mutex::new(Vec::new()),
        }) as Arc<dyn LibraryChangeAudience>);
        let item = movie(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        n.record_added(std::slice::from_ref(&item));
        n.record_removed(&[movie(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4())]);
        tokio::time::sleep(Duration::from_secs(31)).await;

        let published = events.published.lock().expect("lock");
        let changes: Vec<LibraryUpdateInfo> = published
            .iter()
            .filter(|(t, _)| t == ITEMS_CHANGED)
            .map(|(_, p)| serde_json::from_str(p).expect("payload parses"))
            .collect();
        assert_eq!(changes.len(), 1);
        let id = Uuid::parse_str(&item.id)
            .expect("uuid")
            .simple()
            .to_string();
        assert_eq!(changes[0].items_added, [id]);
        assert_eq!(changes[0].items_removed.len(), 1);
        assert!(published.iter().all(|(t, _)| t != "LibraryChanged"));
    }

    // Real clock, not `start_paused`: sqlx's pool acquire times out instantly
    // against a frozen clock. Hence the millisecond debounce.
    #[tokio::test]
    async fn a_repository_save_announces_itself() {
        // The regression this pins: the hook first went on `LibraryManager::
        // update_items`, which only the `POST /Items/{id}` handler calls. Every
        // other writer — provider metadata, user views, media sources — saves
        // through the repository and announced NOTHING, so a client watching a
        // library never learned those items had changed. Measured as h=0 against
        // Jellyfin's j=2 by suite/parity/push.py on 2026-08-31.
        use ferrofin_traits::persistence::ItemPersistenceService;

        let (n, events) = notifier(Duration::from_millis(50));
        let db = crate::test_support::test_db().await;
        let svc = crate::item_persistence_service::FerrofinItemPersistenceService::new(db);
        svc.set_change_notifier(Arc::clone(&n));

        // Parentless: this test is about the hook firing, and a parent/top-parent
        // pointing at rows that do not exist trips the foreign key.
        let mut item = movie(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        item.parent_id = None;
        item.top_parent_id = None;
        svc.save_items(std::slice::from_ref(&item))
            .await
            .expect("save");
        tokio::time::sleep(Duration::from_millis(400)).await;

        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 1, "a repository save must announce itself");
        assert_eq!(
            pushed[0].items_updated,
            vec![
                Uuid::parse_str(&item.id)
                    .expect("uuid")
                    .simple()
                    .to_string()
            ]
        );
    }

    #[tokio::test]
    async fn an_image_update_stamps_the_save_time_and_announces_itself() {
        // `BaseItem.UpdateToRepositoryAsync(ImageUpdate)` after an artwork change:
        // DateLastSaved (the Etag) moves and `ItemUpdated` fires — once, and only
        // for a row that exists.
        use ferrofin_traits::persistence::ItemPersistenceService;

        let (n, events) = notifier(Duration::from_millis(50));
        let db = crate::test_support::test_db().await;
        let svc = crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone());
        let mut item = movie(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        item.parent_id = None;
        item.top_parent_id = None;
        let id = Uuid::parse_str(&item.id).expect("uuid");
        // The stored form (`guid_to_db`), as every real writer supplies.
        item.id = ferrofin_db::store::guid_to_db(id);
        svc.save_items(std::slice::from_ref(&item))
            .await
            .expect("save");
        // Attached after the seed save so only the touch is announced.
        svc.set_change_notifier(Arc::clone(&n));
        let before = crate::test_support::fetch_item(&db, id)
            .await
            .date_last_saved;

        assert!(
            !svc.touch_item_updated(Uuid::new_v4())
                .await
                .expect("missing")
        );
        assert!(svc.touch_item_updated(id).await.expect("touch"));
        tokio::time::sleep(Duration::from_millis(400)).await;

        let after = crate::test_support::fetch_item(&db, id)
            .await
            .date_last_saved;
        assert!(after.is_some() && after != before, "the Etag input moved");
        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 1, "{pushed:?}");
        assert_eq!(pushed[0].items_updated, vec![id.simple().to_string()]);
    }

    // Real clock, as above.
    #[tokio::test]
    async fn a_released_version_announces_itself() {
        // `release_owned_versions` writes outside the save path (the prune
        // frees a part or version whose owner goes); the row it frees is an
        // item of its own from then on, and a client must hear so now.
        use ferrofin_db::store::guid_to_db;
        use ferrofin_traits::persistence::ItemPersistenceService;

        let (n, events) = notifier(Duration::from_millis(50));
        let db = crate::test_support::test_db().await;
        let svc = crate::item_persistence_service::FerrofinItemPersistenceService::new(db);
        // Stored as the scan stores ids (`guid_to_db`), which the release
        // looks rows up by.
        let (primary, id) = (Uuid::new_v4(), Uuid::new_v4());
        let mut owner = movie(primary, Uuid::new_v4(), Uuid::new_v4());
        owner.id = guid_to_db(primary);
        owner.parent_id = None;
        owner.top_parent_id = None;
        let mut part = movie(id, Uuid::new_v4(), Uuid::new_v4());
        part.id = guid_to_db(id);
        part.path = Some("/media/a cd2.mkv".to_owned());
        part.parent_id = None;
        part.top_parent_id = None;
        part.owner_id = Some(guid_to_db(primary));
        svc.save_items(&[owner, part]).await.expect("save");
        svc.set_change_notifier(Arc::clone(&n));

        assert!(svc.release_owned_versions(&[id]).await.expect("release"));
        tokio::time::sleep(Duration::from_millis(400)).await;

        let pushed = events.library_updates();
        assert_eq!(pushed.len(), 1, "a release must announce itself");
        assert_eq!(pushed[0].items_updated, vec![id.simple().to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_metadata_edit_names_no_collection_folder() {
        // `GetTopParentIds` is fed foldersAddedTo + foldersRemovedFrom. An edit
        // moves nothing between folders, so both are empty and C# names NO
        // library. Deriving this from the item's own top parent instead was
        // caught by suite/parity/push.py as a diff at Data.CollectionFolders[].
        let (n, _events) = notifier(Duration::from_secs(30));
        let (lib, alice) = (Uuid::new_v4(), Uuid::new_v4());
        let audience = Arc::new(SplitAudience {
            visible: vec![(alice, lib)],
            locations: Vec::new(),
            hidden: Vec::new(),
            visibility_fails: false,
            delivered: Mutex::new(Vec::new()),
        });
        n.set_audience(Arc::clone(&audience) as Arc<dyn LibraryChangeAudience>);

        let mut item = movie(Uuid::new_v4(), Uuid::new_v4(), lib);
        item.parent_id = None; // an edit in place: no folder gained or lost a child
        n.record_updated(std::slice::from_ref(&item));
        tokio::time::sleep(Duration::from_secs(31)).await;

        let delivered = audience.delivered.lock().expect("lock");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].1.items_updated.len(), 1);
        assert!(
            delivered[0].1.collection_folders.is_empty(),
            "an edit that moved nothing between folders names no library"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_added_item_names_every_library_the_user_can_see() {
        // The other half of `GetTopParentIds`: once ANY folder gained or lost a
        // child, C# names every one of the user's root children -- not just the
        // library the item landed in.
        let (n, _events) = notifier(Duration::from_secs(30));
        let (lib_a, lib_b, alice) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let audience = Arc::new(SplitAudience {
            visible: vec![(alice, lib_a), (alice, lib_b)],
            locations: Vec::new(),
            hidden: Vec::new(),
            visibility_fails: false,
            delivered: Mutex::new(Vec::new()),
        });
        n.set_audience(Arc::clone(&audience) as Arc<dyn LibraryChangeAudience>);

        n.record_added(&[movie(Uuid::new_v4(), Uuid::new_v4(), lib_a)]);
        tokio::time::sleep(Duration::from_secs(31)).await;

        let delivered = audience.delivered.lock().expect("lock");
        let mut want = vec![lib_a.simple().to_string(), lib_b.simple().to_string()];
        want.sort();
        assert_eq!(delivered[0].1.collection_folders, want);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_worth_announcing_pushes_nothing() {
        let (n, events) = notifier(Duration::from_secs(30));
        let mut genre = movie(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        genre.type_ = "MediaBrowser.Controller.Entities.Genre".to_owned();
        n.record_updated(&[genre]);
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert!(events.library_updates().is_empty());
    }
}
