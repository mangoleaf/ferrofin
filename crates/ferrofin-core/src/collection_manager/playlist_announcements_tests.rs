//! What [`FerrofinPlaylistManager`] announces to the library-changed notifier
//! (gl-578): a new playlist is `ItemAdded` (`Folder.AddChild` →
//! `LibraryManager.CreateItem`), and every other committed change is
//! `ItemUpdated` with a new `DateLastSaved` (`UpdatePlaylistInternal` →
//! `UpdateToRepositoryAsync(MetadataEdit)`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities_media::{LibraryUpdateInfo, PlaylistUserPermissions};
use ferrofin_model::playlists::{
    PlaylistCreationRequest, PlaylistUpdateRequest, PlaylistUserUpdateRequest,
};
use ferrofin_traits::collections::PlaylistManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::events::EventManager;
use uuid::Uuid;

use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::library_changed_notifier::LibraryChangedNotifier;
use crate::test_support::{
    fetch_item, item_repository_over, library_manager_over, seed_item, seed_named_user, test_db,
};

use super::FerrofinPlaylistManager;

/// Every `LibraryChanged` the notifier publishes with no audience wired.
#[derive(Default)]
struct Bus(Mutex<Vec<LibraryUpdateInfo>>);

#[async_trait::async_trait]
impl EventManager for Bus {
    async fn publish(&self, event_type: &str, payload: &str) -> Result<(), ServiceError> {
        if event_type == "LibraryChanged" {
            let info = serde_json::from_str(payload).expect("payload parses");
            self.0.lock().expect("lock").push(info);
        }
        Ok(())
    }
}

/// A playlist manager whose announcements reach `bus`.
struct Announcing {
    db: ferrofin_db::Database,
    mgr: FerrofinPlaylistManager,
    notifier: Arc<LibraryChangedNotifier>,
    bus: Arc<Bus>,
    _root: tempfile::TempDir,
}

impl Announcing {
    async fn new() -> Self {
        let db = test_db().await;
        let root = tempfile::tempdir().expect("tempdir");
        let bus = Arc::new(Bus::default());
        // Never fires on its own: each test flushes when it reads.
        let notifier = Arc::new(LibraryChangedNotifier::new(
            Arc::clone(&bus) as Arc<dyn EventManager>,
            Duration::from_secs(3600),
        ));
        let persistence = FerrofinItemPersistenceService::new(db.clone());
        persistence.set_change_notifier(Arc::clone(&notifier));
        let mgr = FerrofinPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            item_repository_over(db.clone()),
            super::tests::test_paths_in(root.path()),
        )
        .with_item_persistence(Arc::new(persistence));
        super::tests::provision_root_folder(&db, &mgr.paths).await;
        Self {
            db,
            mgr,
            notifier,
            bus,
            _root: root,
        }
    }

    /// The pushes since the last read.
    async fn pushes(&self) -> Vec<LibraryUpdateInfo> {
        self.notifier.flush().await;
        std::mem::take(&mut *self.bus.0.lock().expect("lock"))
    }

    async fn create(&self, owner: Uuid, items: Vec<Uuid>) -> Uuid {
        let created = self
            .mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Playlist".to_owned()),
                user_id: owner,
                public: Some(false),
                item_id_list: items,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        Uuid::parse_str(&created.id).expect("uuid")
    }

    /// `DateLastSaved` now, after a pause long enough for the next stamp to
    /// differ.
    async fn saved_at(&self, playlist: Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
        let saved = fetch_item(&self.db, playlist).await.date_last_saved;
        tokio::time::sleep(Duration::from_millis(5)).await;
        saved
    }
}

fn n(id: Uuid) -> String {
    id.simple().to_string()
}

/// Asserts `pushes` is one `ItemsUpdated` naming `playlist` and nothing else,
/// and that its `DateLastSaved` moved past `before`.
async fn assert_updated(
    a: &Announcing,
    playlist: Uuid,
    before: Option<chrono::DateTime<chrono::Utc>>,
    what: &str,
) {
    let pushes = a.pushes().await;
    assert_eq!(pushes.len(), 1, "{what}: {pushes:?}");
    assert_eq!(pushes[0].items_updated, vec![n(playlist)], "{what}");
    assert!(pushes[0].items_added.is_empty(), "{what}: {pushes:?}");
    let after = fetch_item(&a.db, playlist).await.date_last_saved;
    assert!(
        after.is_some() && (before.is_none() || after > before),
        "{what}: DateLastSaved {before:?} -> {after:?}"
    );
}

#[tokio::test]
async fn a_new_playlist_is_announced_as_added() {
    let a = Announcing::new().await;
    let playlist = a.create(Uuid::new_v4(), Vec::new()).await;
    let pushes = a.pushes().await;
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0].items_added, vec![n(playlist)]);
    assert!(pushes[0].items_updated.is_empty(), "{pushes:?}");
}

#[tokio::test]
async fn a_rename_and_a_share_change_are_announced_as_updated() {
    let a = Announcing::new().await;
    let (owner, friend) = (Uuid::new_v4(), Uuid::new_v4());
    let playlist = a.create(owner, Vec::new()).await;
    a.pushes().await;

    let before = a.saved_at(playlist).await;
    a.mgr
        .update_playlist(&PlaylistUpdateRequest {
            id: playlist,
            user_id: owner,
            name: Some("Renamed".to_owned()),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("rename");
    assert_updated(&a, playlist, before, "rename").await;

    let before = a.saved_at(playlist).await;
    a.mgr
        .add_user_to_shares(&PlaylistUserUpdateRequest {
            id: playlist,
            user_id: friend,
            can_edit: Some(true),
        })
        .await
        .expect("share");
    assert_updated(&a, playlist, before, "share").await;

    let before = a.saved_at(playlist).await;
    a.mgr
        .remove_user_from_shares(
            playlist,
            friend,
            &PlaylistUserPermissions::new(friend, true),
        )
        .await
        .expect("unshare");
    assert_updated(&a, playlist, before, "unshare").await;
}

#[tokio::test]
async fn a_move_is_announced_only_when_it_moves_an_entry() {
    let a = Announcing::new().await;
    let owner = Uuid::new_v4();
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    seed_item(&a.db, first, BaseItemKind::Audio).await;
    seed_item(&a.db, second, BaseItemKind::Audio).await;
    let playlist = a.create(owner, vec![first, second]).await;
    a.pushes().await;

    // Already at index 1: nothing changes, nothing is announced.
    a.mgr
        .move_item(&n(playlist), &n(second), 1, owner)
        .await
        .expect("no-op move");
    assert!(a.pushes().await.is_empty());

    let before = a.saved_at(playlist).await;
    a.mgr
        .move_item(&n(playlist), &n(second), 0, owner)
        .await
        .expect("move");
    assert_updated(&a, playlist, before, "move").await;
}

#[tokio::test]
async fn a_playlist_handed_to_a_share_is_announced_as_updated() {
    let a = Announcing::new().await;
    let (owner, heir) = (Uuid::new_v4(), Uuid::new_v4());
    seed_named_user(&a.db, heir, "heir").await;
    let playlist = a.create(owner, Vec::new()).await;
    a.mgr
        .add_user_to_shares(&PlaylistUserUpdateRequest {
            id: playlist,
            user_id: heir,
            can_edit: Some(true),
        })
        .await
        .expect("share");
    a.pushes().await;

    let before = a.saved_at(playlist).await;
    a.mgr.remove_playlists(owner).await.expect("hand over");
    assert_updated(&a, playlist, before, "hand-over").await;
}
