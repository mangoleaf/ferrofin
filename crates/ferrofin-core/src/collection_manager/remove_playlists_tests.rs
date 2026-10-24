//! [`FerrofinPlaylistManager::remove_playlists`]: what deleting a user does
//! to the playlists they own (C# `PlaylistManager.RemovePlaylistsAsync`).

use std::sync::Arc;

use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities_media::PlaylistUserPermissions;
use ferrofin_model::playlists::PlaylistCreationRequest;
use ferrofin_traits::collections::{PlaylistAccessLevel, PlaylistManager};
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::item_data::{json_guid, reconcile_playlist_permissions};
use crate::test_support::{
    item_repository_over, library_manager_over, seed_item, seed_item_with_data, seed_named_user,
    test_db,
};

use super::FerrofinPlaylistManager;

/// A playlist manager over `db`, on a data directory of its own that lives
/// as long as the returned guard.
fn manager(db: &ferrofin_db::Database) -> (FerrofinPlaylistManager, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tempdir");
    let mgr = FerrofinPlaylistManager::new(
        db.clone(),
        library_manager_over(db.clone()),
        item_repository_over(db.clone()),
        Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
            root.path(),
            root.path().join("log"),
            root.path().join("config"),
            root.path().join("cache"),
            root.path().join("web"),
        )),
    );
    (mgr, root)
}

async fn create(
    mgr: &FerrofinPlaylistManager,
    owner: Uuid,
    users: Vec<PlaylistUserPermissions>,
    public: bool,
    items: Vec<Uuid>,
) -> Uuid {
    // The playlists folder a create goes into is startup's to make, and
    // making it again is a no-op.
    super::tests::provision_root_folder(&mgr.db, &mgr.paths).await;
    let created = mgr
        .create_playlist(&PlaylistCreationRequest {
            name: Some("Playlist".to_owned()),
            user_id: owner,
            users,
            public: Some(public),
            item_id_list: items,
            ..PlaylistCreationRequest::default()
        })
        .await
        .expect("create");
    Uuid::parse_str(&created.id).expect("uuid")
}

/// `Users` rows for `users` (`remove_playlists` runs once the deleted user's
/// own row is gone, so only the living get one).
async fn living(db: &ferrofin_db::Database, users: &[Uuid]) {
    for user in users {
        seed_named_user(db, *user, &user.to_string()).await;
    }
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

/// A shared playlist goes to its editing share, whose share row goes; a
/// private one is deleted (its member is not); an open-access one with no
/// share stays; another user's playlist shared with the deleted user is
/// left alone, the deleted user's share included. The hand-over survives
/// the boot re-import of the owners and shares from `Data`.
#[tokio::test]
async fn removing_an_owner_hands_over_shared_playlists_and_deletes_private_ones() {
    let db = test_db().await;
    let (mgr, _root) = manager(&db);
    let (alice, bob, carol) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    living(&db, &[bob, carol]).await;
    let track = Uuid::new_v4();
    seed_item(&db, track, BaseItemKind::Audio).await;
    let shared = create(
        &mgr,
        alice,
        vec![
            PlaylistUserPermissions::new(bob, false),
            PlaylistUserPermissions::new(carol, true),
        ],
        false,
        Vec::new(),
    )
    .await;
    let private = create(&mgr, alice, Vec::new(), false, vec![track]).await;
    let public = create(&mgr, alice, Vec::new(), true, Vec::new()).await;
    let bobs = create(
        &mgr,
        bob,
        vec![PlaylistUserPermissions::new(alice, true)],
        false,
        Vec::new(),
    )
    .await;

    mgr.remove_playlists(alice).await.expect("remove playlists");

    for moment in ["after the removal", "after a restart"] {
        assert_eq!(
            level(&mgr, shared, carol).await,
            Some(PlaylistAccessLevel::Owner),
            "the editing share owns the shared playlist {moment}"
        );
        assert_eq!(
            mgr.get_playlist_shares(shared).await.expect("shares"),
            vec![PlaylistUserPermissions::new(bob, false)],
            "the new owner's share row is gone {moment}"
        );
        assert_eq!(level(&mgr, shared, alice).await, None, "{moment}");
        assert_eq!(
            level(&mgr, public, alice).await,
            Some(PlaylistAccessLevel::Owner),
            "the open-access playlist stays as it was {moment}"
        );
        assert_eq!(
            level(&mgr, bobs, bob).await,
            Some(PlaylistAccessLevel::Owner),
            "another user's playlist keeps its owner {moment}"
        );
        assert_eq!(
            mgr.get_playlist_shares(bobs).await.expect("shares"),
            vec![PlaylistUserPermissions::new(alice, true)],
            "and the deleted user's share on it {moment}"
        );
        reconcile_playlist_permissions(&db)
            .await
            .expect("boot re-import");
    }
    let library = library_manager_over(db.clone());
    assert!(
        library.get_item_by_id(private).await.expect("q").is_none(),
        "the private playlist is deleted"
    );
    assert!(
        library.get_item_by_id(track).await.expect("q").is_some(),
        "its member is not"
    );
}

/// The heir is the first editing share in the order the playlist's `Data`
/// lists its shares (C# `Shares.OrderByDescending(x => x.CanEdit)`, a stable
/// sort), not in user-id order, and a playlist with only read-only shares
/// goes to the first of those: whether or not a boot has imported the
/// playlist's permissions into the tables yet.
#[tokio::test]
async fn the_heir_is_the_first_editor_in_the_order_data_lists_the_shares() {
    let (alice, bob) = (Uuid::from_u128(0xA), Uuid::from_u128(0xB));
    let (carol, dave) = (Uuid::from_u128(0xC), Uuid::from_u128(0xD));
    for imported in [false, true] {
        let db = test_db().await;
        let (mgr, _root) = manager(&db);
        living(&db, &[bob, carol, dave]).await;
        let (editors, readers) = (Uuid::new_v4(), Uuid::new_v4());
        let shares = |users: &[(Uuid, bool)]| {
            users
                .iter()
                .map(|(user, can_edit)| {
                    serde_json::json!({ "UserId": json_guid(*user), "CanEdit": can_edit })
                })
                .collect::<Vec<_>>()
        };
        for (playlist, users) in [
            (
                editors,
                shares(&[(bob, false), (dave, true), (carol, true)]),
            ),
            (readers, shares(&[(dave, false), (carol, false)])),
        ] {
            let data = serde_json::json!({
                "OwnerUserId": json_guid(alice),
                "OpenAccess": false,
                "Shares": users,
            });
            seed_item_with_data(
                &db,
                playlist,
                BaseItemKind::Playlist,
                "P",
                &data.to_string(),
            )
            .await;
        }
        if imported {
            reconcile_playlist_permissions(&db).await.expect("boot");
        }

        mgr.remove_playlists(alice).await.expect("remove playlists");

        assert_eq!(
            level(&mgr, editors, dave).await,
            Some(PlaylistAccessLevel::Owner),
            "imported: {imported}"
        );
        // `Shares = rankedShares.Skip(1)`: the rest, editors first.
        assert_eq!(
            mgr.get_playlist_shares(editors).await.expect("shares"),
            vec![
                PlaylistUserPermissions::new(carol, true),
                PlaylistUserPermissions::new(bob, false),
            ],
            "imported: {imported}"
        );
        assert_eq!(
            level(&mgr, readers, dave).await,
            Some(PlaylistAccessLevel::Owner),
            "imported: {imported}"
        );
        assert_eq!(
            mgr.get_playlist_shares(readers).await.expect("shares"),
            vec![PlaylistUserPermissions::new(carol, false)],
            "imported: {imported}"
        );
    }
}

/// The share of a user who no longer exists never inherits: the first living
/// share does, and a private playlist whose only share is such a user's is
/// deleted. Upstream would hand both to the missing user and leave them owned
/// by nobody.
#[tokio::test]
async fn the_share_of_a_user_who_no_longer_exists_never_inherits() {
    let db = test_db().await;
    let (mgr, _root) = manager(&db);
    let (alice, gone, bob) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    living(&db, &[bob]).await;
    let shared = create(
        &mgr,
        alice,
        vec![
            PlaylistUserPermissions::new(gone, true),
            PlaylistUserPermissions::new(bob, false),
        ],
        false,
        Vec::new(),
    )
    .await;
    let only_gone = create(
        &mgr,
        alice,
        vec![PlaylistUserPermissions::new(gone, true)],
        false,
        Vec::new(),
    )
    .await;

    mgr.remove_playlists(alice).await.expect("remove playlists");

    assert_eq!(
        level(&mgr, shared, bob).await,
        Some(PlaylistAccessLevel::Owner)
    );
    assert_eq!(
        mgr.get_playlist_shares(shared).await.expect("shares"),
        vec![PlaylistUserPermissions::new(gone, true)],
        "the missing user's share stays, as upstream leaves a deleted user's shares"
    );
    let library = library_manager_over(db.clone());
    assert!(
        library
            .get_item_by_id(only_gone)
            .await
            .expect("q")
            .is_none(),
        "a private playlist with no living share is deleted"
    );
}

/// A playlist whose hand-over fails does not stop the others: they are
/// still handed over, the failed one keeps its owner (its transaction rolls
/// back), and the call reports the failure.
#[tokio::test]
async fn a_failed_hand_over_does_not_stop_the_others() {
    let db = test_db().await;
    let (mgr, _root) = manager(&db);
    let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
    living(&db, &[bob]).await;
    let shares = || vec![PlaylistUserPermissions::new(bob, true)];
    let failing = create(&mgr, alice, shares(), false, Vec::new()).await;
    let fine = create(&mgr, alice, shares(), false, Vec::new()).await;
    crate::item_data::fail_data_writes_of(&db, failing).await;

    mgr.remove_playlists(alice)
        .await
        .expect_err("the failed hand-over is reported");

    assert_eq!(
        level(&mgr, fine, bob).await,
        Some(PlaylistAccessLevel::Owner)
    );
    assert_eq!(
        level(&mgr, failing, alice).await,
        Some(PlaylistAccessLevel::Owner),
        "the failed hand-over changed nothing"
    );
}
