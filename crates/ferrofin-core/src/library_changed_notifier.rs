//! The debounced `LibraryChanged` push — port of
//! `Emby.Server.Implementations/EntryPoints/LibraryChangedNotifier.cs`.
//!
//! Jellyfin's notifier subscribes to `ILibraryManager.ItemAdded` /
//! `ItemUpdated` / `ItemRemoved`, accumulates the changed items, and resets a
//! one-shot timer on every change. When the timer finally fires — nothing has
//! changed for `LibraryUpdateDuration` seconds — it folds the accumulated sets
//! into a [`LibraryUpdateInfo`] and pushes it so open clients refresh their
//! views.
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

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::entities_media::LibraryUpdateInfo;
use ferrofin_traits::events::{EventManager, LibraryChangeAudience};
use uuid::Uuid;

/// Jellyfin's `ServerConfiguration.LibraryUpdateDuration` default, in seconds
/// (`configuration_manager.rs` seeds the same 30).
pub const DEFAULT_LIBRARY_UPDATE_DURATION_SECS: u64 = 30;

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
    /// The parent folder, which the added/removed buckets report separately.
    parent: Option<String>,
}

/// What has changed since the last flush.
#[derive(Debug, Default)]
struct Pending {
    added: Vec<Changed>,
    updated: Vec<Changed>,
    removed: Vec<Changed>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

/// Accumulates library changes and pushes a folded `LibraryChanged` once the
/// changes stop arriving.
pub struct LibraryChangedNotifier {
    events: Arc<dyn EventManager>,
    debounce: Duration,
    pending: Mutex<Pending>,
    /// Bumped by every `record_*`. The timer task that wakes to find the
    /// generation moved on has been superseded by a later change and returns
    /// without flushing — the Rust shape of `Timer.Change(duration, Infinite)`,
    /// which likewise restarts rather than stacks.
    generation: AtomicU64,
    /// The per-user fan-out. Set by the composition root after the library
    /// manager it reads from exists. `None` — unit tests, and any moment
    /// before wiring completes — falls back to publishing one payload on the
    /// event bus, which is what the scan-end push has always done.
    audience: std::sync::OnceLock<Arc<dyn LibraryChangeAudience>>,
}

impl LibraryChangedNotifier {
    /// Creates a notifier that publishes through `events` after `debounce`
    /// of quiet.
    #[must_use]
    pub fn new(events: Arc<dyn EventManager>, debounce: Duration) -> Self {
        Self {
            events,
            debounce,
            pending: Mutex::new(Pending::default()),
            generation: AtomicU64::new(0),
            audience: std::sync::OnceLock::new(),
        }
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

    /// Records created items (`ItemAdded`) and restarts the debounce.
    pub fn record_added(self: &Arc<Self>, items: &[BaseItemEntity]) {
        self.record(items, |p| &mut p.added);
    }

    /// Records saved items (`ItemUpdated`) and restarts the debounce.
    pub fn record_updated(self: &Arc<Self>, items: &[BaseItemEntity]) {
        self.record(items, |p| &mut p.updated);
    }

    /// Records deleted items (`ItemRemoved`) and restarts the debounce.
    ///
    /// Takes the rows rather than bare ids because the flush needs the owning
    /// library and parent folder, which are unresolvable once the rows are gone.
    pub fn record_removed(self: &Arc<Self>, items: &[BaseItemEntity]) {
        self.record(items, |p| &mut p.removed);
    }

    fn record(
        self: &Arc<Self>,
        items: &[BaseItemEntity],
        bucket: fn(&mut Pending) -> &mut Vec<Changed>,
    ) {
        let changed: Vec<Changed> = items
            .iter()
            .filter(|i| Self::filter_item(i))
            .filter_map(Self::changed_of)
            .collect();
        if changed.is_empty() {
            return;
        }
        {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            bucket(&mut pending).extend(changed);
        }
        self.restart_timer();
    }

    /// Restarts the one-shot debounce (`_libraryUpdateTimer.Change`).
    fn restart_timer(self: &Arc<Self>) {
        let mine = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(this.debounce).await;
            // A change landed while this task slept, so a later task owns the
            // flush. Returning here is what makes the timer restart rather
            // than fire once per recorded change.
            if this.generation.load(Ordering::SeqCst) != mine {
                return;
            }
            this.flush().await;
        });
    }

    /// Folds the accumulated changes and publishes them (`LibraryUpdateTimerCallback`).
    ///
    /// Public so a caller that needs the push *now* — a test, or a shutdown
    /// path that must not lose a pending announcement — can force it.
    pub async fn flush(&self) {
        let pending = {
            let Ok(mut guard) = self.pending.lock() else {
                return;
            };
            if guard.is_empty() {
                return;
            }
            std::mem::take(&mut *guard)
        };
        let folded = fold(&pending);
        if folded.info.is_empty {
            return;
        }
        let Some(audience) = self.audience.get() else {
            // No fan-out wired: one payload to everyone, the pre-existing
            // scan-end behaviour.
            if let Ok(payload) = serde_json::to_string(&folded.info) {
                let _ = self.events.publish("LibraryChanged", &payload).await;
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
            let visible: HashSet<String> =
                visible.iter().map(|id| id.simple().to_string()).collect();
            let info = folded.for_libraries(&visible);
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

/// The deduped change set, kept alongside the payload it folds into.
///
/// The per-user filter needs each item's owning library, which the wire
/// [`LibraryUpdateInfo`] does not carry — it is a flat list of ids. So the
/// folded buckets are retained and [`Folded::for_libraries`] re-derives one
/// user's payload from them.
struct Folded {
    added: Vec<Changed>,
    updated: Vec<Changed>,
    removed: Vec<Changed>,
    /// The unfiltered payload, used when no per-user audience is wired.
    info: LibraryUpdateInfo,
}

impl Folded {
    /// One user's `GetLibraryUpdateInfo`, filtered to the libraries they can see.
    ///
    /// Removals are NOT filtered. C# passes `includeIfNotFound: true` for them
    /// alone, because the row is already gone and `IsVisibleStandalone` has
    /// nothing left to test — telling a client to drop an id it never had is
    /// harmless, whereas withholding it leaves a deleted item on screen.
    fn for_libraries(&self, visible: &HashSet<String>) -> LibraryUpdateInfo {
        let keep = |v: &[Changed]| -> Vec<Changed> {
            v.iter()
                .filter(|c| c.top_parent.as_ref().is_some_and(|t| visible.contains(t)))
                .cloned()
                .collect()
        };
        build(
            &keep(&self.added),
            &keep(&self.updated),
            &self.removed,
            Some(visible),
        )
    }
}

/// Builds the wire payload from three already-deduped buckets.
///
/// `user_libraries` is the set of libraries the recipient can see, when there is
/// a recipient. It is what `CollectionFolders` is derived from — see below.
fn build(
    added: &[Changed],
    updated: &[Changed],
    removed: &[Changed],
    user_libraries: Option<&HashSet<String>>,
) -> LibraryUpdateInfo {
    let ids = |v: &[Changed]| -> Vec<String> {
        let mut seen = HashSet::new();
        v.iter()
            .filter(|c| seen.insert(c.id.clone()))
            .map(|c| c.id.clone())
            .collect()
    };
    let parents = |v: &[Changed]| -> Vec<String> {
        let mut seen = HashSet::new();
        v.iter()
            .filter_map(|c| c.parent.clone())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    };
    let folders_added_to = parents(added);
    let folders_removed_from = parents(removed);

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
    let collection_folders: Vec<String> = match user_libraries {
        Some(libs) if !folders_added_to.is_empty() || !folders_removed_from.is_empty() => {
            let mut v: Vec<String> = libs.iter().cloned().collect();
            v.sort();
            v
        }
        Some(_) => Vec::new(),
        // No recipient to scope to (no audience wired): fall back to the
        // libraries the changed items belong to, which is the most a broadcast
        // can honestly say.
        None => {
            let mut seen = HashSet::new();
            added
                .iter()
                .chain(removed)
                .chain(updated)
                .filter_map(|c| c.top_parent.clone())
                .filter(|t| seen.insert(t.clone()))
                .collect()
        }
    };

    let mut info = LibraryUpdateInfo {
        folders_added_to,
        folders_removed_from,
        items_added: ids(added),
        items_removed: ids(removed),
        items_updated: ids(updated),
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
    let added_ids: HashSet<&String> = pending.added.iter().map(|c| &c.id).collect();
    let updated: Vec<Changed> = pending
        .updated
        .iter()
        .filter(|c| !added_ids.contains(&c.id))
        .cloned()
        .collect();
    let info = build(&pending.added, &updated, &pending.removed, None);
    Folded {
        added: pending.added.clone(),
        updated,
        removed: pending.removed.clone(),
        info,
    }
}

impl LibraryChangedNotifier {
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
        if all.is_empty() {
            return;
        }
        {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            pending.removed.extend(all);
        }
        self.restart_timer();
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

    #[tokio::test(start_paused = true)]
    async fn changes_are_coalesced_into_one_push_after_the_quiet_period() {
        let (n, events) = notifier(Duration::from_secs(30));
        let library = Uuid::new_v4();
        let parent = Uuid::new_v4();

        // Three writes 10s apart: each restarts the timer, so none of them
        // fires on its own. A per-change timer would have pushed three times.
        for _ in 0..3 {
            n.record_added(&[movie(Uuid::new_v4(), parent, library)]);
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        assert!(
            events.library_updates().is_empty(),
            "the debounce must restart on each change, not fire 30s after the first"
        );

        tokio::time::sleep(Duration::from_secs(31)).await;
        let pushed = events.library_updates();
        assert_eq!(
            pushed.len(),
            1,
            "one push for the whole quiet-bounded batch"
        );
        assert_eq!(pushed[0].items_added.len(), 3);
        assert_eq!(
            pushed[0].collection_folders,
            vec![library.simple().to_string()]
        );
        assert!(!pushed[0].is_empty);
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
        let audience = Arc::new(SplitAudience {
            visible: vec![(alice, lib_a), (bob, lib_b)],
            delivered: Mutex::new(Vec::new()),
        });
        n.set_audience(Arc::clone(&audience) as Arc<dyn LibraryChangeAudience>);

        // One item added to library A only.
        let item = movie(Uuid::new_v4(), Uuid::new_v4(), lib_a);
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
