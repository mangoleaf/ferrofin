//! [`super::remove_orphaned_playlists`]: the playlists of owners who no
//! longer exist, disposed of once at boot as if the owner had just been
//! deleted (gl-577).

use std::path::Path;
use std::sync::Arc;

use ferrofin_db::Database;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities_media::PlaylistUserPermissions;
use ferrofin_traits::collections::{PlaylistAccessLevel, PlaylistManager};
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use super::remove_orphaned_playlists;
use crate::FerrofinPlaylistManager;
use crate::item_data::{
    allow_data_writes, fail_data_writes_of, json_guid, reconcile_playlist_permissions,
    reimport_from_data,
};
use crate::test_support::{
    item_repository_over, library_manager_over, seed_item_with_data, seed_named_user, test_db,
};

/// The repair's done-marker in `FerrofinMeta`.
const MARKER: &str = "orphaned_playlist_owners_v1";

/// A migrated database at `<dir>/jellyfin.db`.
async fn file_db(dir: &Path) -> Database {
    let db = file_db_at(&dir.join("jellyfin.db")).await;
    db.run_migrations().await.expect("migrate");
    db
}

/// The database file at `path`, opened as it is.
async fn file_db_at(path: &Path) -> Database {
    Database::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("connect")
}

/// A playlist manager over `db`, its data directory under `root`.
fn manager(db: &Database, root: &Path) -> FerrofinPlaylistManager {
    FerrofinPlaylistManager::new(
        db.clone(),
        library_manager_over(db.clone()),
        item_repository_over(db.clone()),
        Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
            root,
            root.join("log"),
            root.join("config"),
            root.join("cache"),
            root.join("web"),
        )),
    )
}

/// A playlist as a database holds one before a boot imports it: its owner
/// (none when `None`), open access and shares only in `Data`.
async fn playlist(
    db: &Database,
    owner: Option<Uuid>,
    shares: &[(Uuid, bool)],
    open_access: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    let shares: Vec<_> = shares
        .iter()
        .map(|(user, can_edit)| {
            serde_json::json!({ "UserId": json_guid(*user), "CanEdit": can_edit })
        })
        .collect();
    let mut data = serde_json::json!({ "OpenAccess": open_access, "Shares": shares });
    if let Some(owner) = owner {
        data["OwnerUserId"] = json_guid(owner).into();
    }
    seed_item_with_data(db, id, BaseItemKind::Playlist, "P", &data.to_string()).await;
    id
}

async fn level(
    mgr: &FerrofinPlaylistManager,
    playlist: Uuid,
    user: Uuid,
) -> Option<PlaylistAccessLevel> {
    match mgr.get_playlist_access(playlist, user).await {
        Ok(access) => Some(access.level),
        Err(ServiceError::NotFound(_)) => None,
        Err(error) => panic!("access: {error}"),
    }
}

/// A missing owner's shared playlist goes to its editing share, else its
/// first share; a private one is deleted; an open-access one stays as it
/// is. Playlists of a user who exists, and unowned ones (no owner, or the
/// empty GUID), are left alone. The file is copied aside first, the result
/// is in `Data` too (the tables `Data` alone gives back are the same), and
/// the repair is marked done.
#[tokio::test]
async fn orphaned_playlists_are_disposed_of_as_if_their_owner_had_just_been_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(dir.path()).await;
    let mgr = manager(&db, dir.path());
    let (alice, bob, carol) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_named_user(&db, bob, "bob").await;
    seed_named_user(&db, carol, "carol").await;
    let shared = playlist(&db, Some(alice), &[(bob, false), (carol, true)], false).await;
    let read_only = playlist(&db, Some(alice), &[(bob, false)], false).await;
    let private = playlist(&db, Some(alice), &[], false).await;
    let public = playlist(&db, Some(alice), &[], true).await;
    let bobs = playlist(&db, Some(bob), &[], false).await;
    let no_owner = playlist(&db, None, &[], false).await;
    let empty_owner = playlist(&db, Some(Uuid::nil()), &[], false).await;
    reconcile_playlist_permissions(&db).await.expect("boot");

    assert_eq!(
        remove_orphaned_playlists(&db, &mgr).await.expect("repair"),
        1,
        "one missing owner"
    );

    let snapshot = file_db_at(&dir.path().join("jellyfin.db.pre-orphaned-playlists")).await;
    assert!(
        library_manager_over(snapshot)
            .get_item_by_id(private)
            .await
            .expect("q")
            .is_some(),
        "the copy taken first still holds the private playlist"
    );
    for moment in ["after the repair", "from `Data` alone"] {
        assert_eq!(
            level(&mgr, shared, carol).await,
            Some(PlaylistAccessLevel::Owner),
            "the editing share owns the shared playlist {moment}"
        );
        assert_eq!(
            mgr.get_playlist_shares(shared).await.expect("shares"),
            vec![PlaylistUserPermissions::new(bob, false)],
            "{moment}"
        );
        assert_eq!(
            level(&mgr, read_only, bob).await,
            Some(PlaylistAccessLevel::Owner),
            "a read-only share inherits when it is the only one {moment}"
        );
        assert_eq!(
            level(&mgr, public, alice).await,
            Some(PlaylistAccessLevel::Owner),
            "the open-access playlist stays as it was {moment}"
        );
        assert_eq!(
            level(&mgr, bobs, bob).await,
            Some(PlaylistAccessLevel::Owner),
            "{moment}"
        );
        for playlist in [shared, read_only, public, bobs] {
            reimport_from_data(&db, playlist).await;
        }
    }
    let library = library_manager_over(db.clone());
    assert!(
        library.get_item_by_id(private).await.expect("q").is_none(),
        "the private playlist is deleted"
    );
    for kept in [public, bobs, no_owner, empty_owner] {
        assert!(
            library.get_item_by_id(kept).await.expect("q").is_some(),
            "{kept} is kept"
        );
    }
    assert!(db.meta_get(MARKER).await.expect("meta").is_some());
}

/// With no missing owner nothing is copied aside, and the repair is done.
#[tokio::test]
async fn without_orphans_no_snapshot_is_taken() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(dir.path()).await;
    let mgr = manager(&db, dir.path());
    let bob = Uuid::new_v4();
    seed_named_user(&db, bob, "bob").await;
    playlist(&db, Some(bob), &[], false).await;
    reconcile_playlist_permissions(&db).await.expect("boot");

    assert_eq!(
        remove_orphaned_playlists(&db, &mgr).await.expect("repair"),
        0
    );

    assert!(
        !dir.path()
            .join("jellyfin.db.pre-orphaned-playlists")
            .exists()
    );
    assert!(db.meta_get(MARKER).await.expect("meta").is_some());
}

/// With owned playlists but no user to be read, every owner would look
/// missing: nothing is copied or changed and the repair stays undone. With
/// no owned playlist either (a fresh install, whose administrator is seeded
/// after the repairs run) it is simply done.
#[tokio::test]
async fn without_a_user_nothing_is_disposed_of() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(dir.path()).await;
    let mgr = manager(&db, dir.path());
    playlist(&db, None, &[], false).await;
    reconcile_playlist_permissions(&db).await.expect("boot");
    assert_eq!(
        remove_orphaned_playlists(&db, &mgr).await.expect("fresh"),
        0
    );
    assert!(db.meta_get(MARKER).await.expect("meta").is_some());

    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(dir.path()).await;
    let mgr = manager(&db, dir.path());
    let private = playlist(&db, Some(Uuid::new_v4()), &[], false).await;
    reconcile_playlist_permissions(&db).await.expect("boot");
    assert_eq!(
        remove_orphaned_playlists(&db, &mgr).await.expect("repair"),
        0
    );
    assert!(
        library_manager_over(db.clone())
            .get_item_by_id(private)
            .await
            .expect("q")
            .is_some()
    );
    assert!(
        !dir.path()
            .join("jellyfin.db.pre-orphaned-playlists")
            .exists()
    );
    assert!(db.meta_get(MARKER).await.expect("meta").is_none());
}

/// A snapshot that cannot be written (something else at its name) fails the
/// repair before it changes anything, and leaves it undone.
#[tokio::test]
async fn nothing_changes_without_the_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(dir.path()).await;
    let mgr = manager(&db, dir.path());
    let bob = Uuid::new_v4();
    seed_named_user(&db, bob, "bob").await;
    let private = playlist(&db, Some(Uuid::new_v4()), &[], false).await;
    reconcile_playlist_permissions(&db).await.expect("boot");
    std::fs::create_dir(dir.path().join("jellyfin.db.pre-orphaned-playlists")).expect("dir");

    remove_orphaned_playlists(&db, &mgr)
        .await
        .expect_err("no snapshot");

    assert!(
        library_manager_over(db.clone())
            .get_item_by_id(private)
            .await
            .expect("q")
            .is_some()
    );
    assert!(db.meta_get(MARKER).await.expect("meta").is_none());
}

/// A hand-over that fails (as one can in `DELETE /Users/{id}` once the user
/// row is gone, gl-131) does not stop the others, another missing owner's
/// included, and leaves the repair undone, so the next boot hands that
/// playlist over. Once done, it never
/// runs again: a playlist orphaned since stays as it is.
#[tokio::test]
async fn a_failed_hand_over_is_retried_by_the_next_boot_and_then_never_again() {
    let db = test_db().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let mgr = manager(&db, dir.path());
    let (alice, bob, dave) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_named_user(&db, bob, "bob").await;
    let failing = playlist(&db, Some(alice), &[(bob, true)], false).await;
    let fine = playlist(&db, Some(alice), &[(bob, true)], false).await;
    let daves = playlist(&db, Some(dave), &[], false).await;
    reconcile_playlist_permissions(&db).await.expect("boot");
    fail_data_writes_of(&db, failing).await;

    assert_eq!(
        remove_orphaned_playlists(&db, &mgr).await.expect("repair"),
        1,
        "dave's playlists are disposed of, alice's not all"
    );
    assert!(
        library_manager_over(db.clone())
            .get_item_by_id(daves)
            .await
            .expect("q")
            .is_none()
    );

    assert_eq!(
        level(&mgr, fine, bob).await,
        Some(PlaylistAccessLevel::Owner)
    );
    assert_eq!(
        level(&mgr, failing, alice).await,
        Some(PlaylistAccessLevel::Owner),
        "the failed hand-over changed nothing"
    );
    assert!(db.meta_get(MARKER).await.expect("meta").is_none());

    allow_data_writes(&db).await;
    assert_eq!(
        remove_orphaned_playlists(&db, &mgr)
            .await
            .expect("next boot"),
        1
    );
    assert_eq!(
        level(&mgr, failing, bob).await,
        Some(PlaylistAccessLevel::Owner)
    );
    assert!(db.meta_get(MARKER).await.expect("meta").is_some());

    let later = playlist(&db, Some(Uuid::new_v4()), &[(bob, true)], false).await;
    reconcile_playlist_permissions(&db).await.expect("boot");
    assert_eq!(
        remove_orphaned_playlists(&db, &mgr)
            .await
            .expect("a later boot"),
        0
    );
    assert_eq!(
        mgr.get_playlist_shares(later).await.expect("shares"),
        vec![PlaylistUserPermissions::new(bob, true)],
        "a playlist orphaned after the repair is left as it is"
    );
}
