//! The user library scope's playlists and box-set gate: v12.2
//! `LibraryManager.AddUserToQuery` scopes an unscoped query to the top parents
//! of `UserViewManager.GetUserViews(user)`, which keeps the playlists folder
//! and a `boxsets` library only while `HasVisibleItem` finds a `Playlist`
//! (`BoxSet`) the user can see (`UserViewManager.cs:62-73,161-185`). The
//! playlists folder's own `TopParentId` is its id, so without the gate a user
//! with no playlist got the folder in every unscoped recursive `/Items`.

use super::*;
use crate::item_count_service::FerrofinItemCountService;
use crate::item_type_lookup::ItemTypeLookup;
use crate::test_support::{save_item, seed_named_user, seed_user_with_defaults, test_db};
use ferrofin_traits::persistence::{ItemCountService, LinkedChildrenService};

/// `ItemVisibility`'s data directory: a playlist under it is a shared
/// (`Playlist.IsSharedItem`) one, visible by owner, shares and `OpenAccess`.
const DATA: &str = "/config/data";

const USER: Uuid = Uuid::from_u128(0x0826_0001);
const OWNER: Uuid = Uuid::from_u128(0x0826_0002);
const MOVIES: Uuid = Uuid::from_u128(0x0826_0010);
const MOVIE: Uuid = Uuid::from_u128(0x0826_0011);
const PLAYLISTS: Uuid = Uuid::from_u128(0x0826_0020);
const PLAYLIST: Uuid = Uuid::from_u128(0x0826_0021);
const HIDDEN_LIBRARY: Uuid = Uuid::from_u128(0x0826_0030);
const HIDDEN_MOVIE: Uuid = Uuid::from_u128(0x0826_0031);
const COLLECTIONS: Uuid = Uuid::from_u128(0x0826_0040);
const HIDDEN_SET: Uuid = Uuid::from_u128(0x0826_0041);
const OPEN_SET: Uuid = Uuid::from_u128(0x0826_0042);

/// A repository and a count service wired as the composition root wires
/// them: both reach the server's `ItemVisibility` through one late slot.
/// The evaluator is returned so the slot's weak reference stays live.
struct Wired {
    db: Database,
    repository: Arc<FerrofinItemRepository>,
    counts: FerrofinItemCountService,
    _visibility: Arc<ItemVisibility>,
}

impl Wired {
    fn new(db: Database) -> Self {
        let slot = VisibilitySlot::default();
        let repository = Arc::new(
            FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
                .with_visibility(Arc::clone(&slot)),
        );
        let visibility = Arc::new(ItemVisibility::new(
            db.clone(),
            Arc::clone(&repository) as Arc<dyn ItemRepository>,
            Arc::new(crate::localization_manager::LocalizationManager::new("US")),
            DATA.to_owned(),
        ));
        assert!(slot.set(Arc::downgrade(&visibility)).is_ok());
        let counts = FerrofinItemCountService::new(db.clone()).with_visibility(slot);
        Self {
            db,
            repository,
            counts,
            _visibility: visibility,
        }
    }

    /// `/Items?userId=…&recursive=true[&includeItemTypes=…]`: the names it
    /// lists, sorted, and the count service's answer for the same query.
    async fn unscoped(&self, kinds: &[BaseItemKind]) -> (Vec<String>, i32) {
        let user = query_user(&self.db, USER)
            .await
            .expect("user")
            .expect("seeded");
        let query = InternalItemsQuery {
            user: Some(user),
            recursive: true,
            include_item_types: kinds.to_vec(),
            ..InternalItemsQuery::default()
        };
        let mut names: Vec<String> = self
            .repository
            .get_item_list(&query)
            .await
            .expect("query")
            .into_iter()
            .filter_map(|row| row.name)
            .collect();
        names.sort();
        let count = self.counts.get_count(&query).await.expect("count");
        (names, count)
    }
}

/// Saves one row through the real upsert.
async fn put(
    db: &Database,
    id: Uuid,
    (kind, name): (BaseItemKind, &str),
    (parent, top_parent): (Option<Uuid>, Uuid),
    path: &str,
    data: &serde_json::Value,
) {
    save_item(
        db,
        &BaseItemEntity {
            id: guid_to_db(id),
            type_: stored_type_name(kind).unwrap_or_default().to_owned(),
            name: Some(name.to_owned()),
            sort_name: Some(name.to_owned()),
            parent_id: parent.map(guid_to_db),
            top_parent_id: Some(guid_to_db(top_parent)),
            path: Some(path.to_owned()),
            is_folder: crate::kinds::is_folder(kind),
            data: Some(data.to_string()),
            ..BaseItemEntity::default()
        },
    )
    .await;
}

/// The user, another user, and a movie library holding one movie.
async fn library() -> Database {
    let db = test_db().await;
    seed_user_with_defaults(&db, USER).await;
    seed_named_user(&db, OWNER, "owner").await;
    put(
        &db,
        MOVIES,
        (BaseItemKind::CollectionFolder, "Movies"),
        (None, MOVIES),
        "/movies",
        &serde_json::json!({"CollectionType": "movies"}),
    )
    .await;
    put(
        &db,
        MOVIE,
        (BaseItemKind::Movie, "A Movie"),
        (Some(MOVIES), MOVIES),
        "/movies/A Movie.mkv",
        &serde_json::json!({}),
    )
    .await;
    db
}

/// The playlists folder (its own top parent, as `CreateRootFolder` saves it)
/// holding another user's playlist with `data` as its sharing.
async fn with_playlist(db: &Database, data: &serde_json::Value) {
    put(
        db,
        PLAYLISTS,
        (BaseItemKind::PlaylistsFolder, "Playlists"),
        (None, PLAYLISTS),
        &format!("{DATA}/playlists"),
        &serde_json::json!({}),
    )
    .await;
    put(
        db,
        PLAYLIST,
        (BaseItemKind::Playlist, "Theirs"),
        (Some(PLAYLISTS), PLAYLISTS),
        &format!("{DATA}/playlists/Theirs"),
        data,
    )
    .await;
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

/// A user who can see no playlist has no playlists folder in scope: neither
/// the folder nor another user's private playlist under it is listed or
/// counted, and a typed `PlaylistsFolder` request answers nothing.
#[tokio::test]
async fn a_user_with_no_visible_playlist_has_no_playlists_folder_in_scope() {
    let db = library().await;
    with_playlist(
        &db,
        &serde_json::json!({"OwnerUserId": OWNER, "OpenAccess": false}),
    )
    .await;
    let wired = Wired::new(db);

    let (listed, count) = wired.unscoped(&[]).await;
    assert_eq!(listed, names(&["A Movie", "Movies"]));
    assert_eq!(count, 2);

    let (listed, count) = wired.unscoped(&[BaseItemKind::PlaylistsFolder]).await;
    assert!(listed.is_empty(), "got {listed:?}");
    assert_eq!(count, 0);
}

/// Once a playlist is shared with the user, the folder is one of their
/// views again, and the playlists parented to it are back in scope.
#[tokio::test]
async fn a_user_who_sees_a_playlist_keeps_the_folder_and_its_playlists() {
    let db = library().await;
    with_playlist(
        &db,
        &serde_json::json!({
            "OwnerUserId": OWNER,
            "OpenAccess": false,
            "Shares": [{"UserId": USER, "CanEdit": false}],
        }),
    )
    .await;
    let wired = Wired::new(db);

    let (listed, count) = wired.unscoped(&[]).await;
    assert_eq!(listed, names(&["A Movie", "Movies", "Playlists", "Theirs"]));
    assert_eq!(count, 4);

    let (listed, count) = wired.unscoped(&[BaseItemKind::PlaylistsFolder]).await;
    assert_eq!(listed, names(&["Playlists"]));
    assert_eq!(count, 1);
}

/// The boxsets half: a "Collections" library whose only box set links to a
/// library the user may not open is not one of the user's views, so nothing
/// under it is in scope; a box set the user can see brings it back.
#[tokio::test]
async fn a_boxsets_library_is_in_scope_only_with_a_box_set_the_user_can_see() {
    let db = library().await;
    put(
        &db,
        HIDDEN_LIBRARY,
        (BaseItemKind::CollectionFolder, "Hidden"),
        (None, HIDDEN_LIBRARY),
        "/hidden",
        &serde_json::json!({"CollectionType": "movies"}),
    )
    .await;
    put(
        &db,
        HIDDEN_MOVIE,
        (BaseItemKind::Movie, "Hidden Movie"),
        (Some(HIDDEN_LIBRARY), HIDDEN_LIBRARY),
        "/hidden/Hidden Movie.mkv",
        &serde_json::json!({}),
    )
    .await;
    crate::user_entity_ext::set_preference(
        db.pool(),
        &guid_to_db(USER),
        PreferenceKind::BlockedMediaFolders,
        &[guid_to_db(HIDDEN_LIBRARY)],
    )
    .await
    .expect("block the hidden library");
    put(
        &db,
        COLLECTIONS,
        (BaseItemKind::CollectionFolder, "Collections"),
        (None, COLLECTIONS),
        &format!("{DATA}/collections"),
        &serde_json::json!({"CollectionType": "boxsets"}),
    )
    .await;
    put(
        &db,
        HIDDEN_SET,
        (BaseItemKind::BoxSet, "Hidden Set"),
        (Some(COLLECTIONS), COLLECTIONS),
        &format!("{DATA}/collections/Hidden Set"),
        &serde_json::json!({}),
    )
    .await;
    crate::FerrofinLinkedChildrenService::new(db.clone())
        .upsert_linked_child(HIDDEN_SET, HIDDEN_MOVIE, 0)
        .await
        .expect("link");
    let wired = Wired::new(db.clone());

    let (listed, count) = wired.unscoped(&[]).await;
    assert_eq!(listed, names(&["A Movie", "Movies"]));
    assert_eq!(count, 2);

    put(
        &db,
        OPEN_SET,
        (BaseItemKind::BoxSet, "Open Set"),
        (Some(COLLECTIONS), COLLECTIONS),
        &format!("{DATA}/collections/Open Set"),
        &serde_json::json!({}),
    )
    .await;
    let (listed, _) = wired.unscoped(&[]).await;
    assert!(
        listed.contains(&"Open Set".to_owned()) && listed.contains(&"Collections".to_owned()),
        "a visible box set puts the library back in scope, got {listed:?}"
    );
}

/// Without an evaluator bound (a repository built without
/// `with_visibility`) the scope keeps both linked libraries, as before.
#[tokio::test]
async fn without_an_evaluator_the_scope_keeps_the_playlists_folder() {
    let db = library().await;
    with_playlist(
        &db,
        &serde_json::json!({"OwnerUserId": OWNER, "OpenAccess": false}),
    )
    .await;
    let user = query_user(&db, USER).await.expect("user").expect("seeded");
    let listed: Vec<String> =
        FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
            .get_item_list(&InternalItemsQuery {
                user: Some(user),
                recursive: true,
                include_item_types: vec![BaseItemKind::PlaylistsFolder],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("query")
            .into_iter()
            .filter_map(|row| row.name)
            .collect();
    assert_eq!(listed, names(&["Playlists"]));
}
