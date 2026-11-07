//! A playlist edit writes its entries in one statement, whatever the length.
//!
//! An add, a remove and a move each rewrite the playlist's `LinkedChildren`
//! rows, as upstream's save does (delete the parent's rows, re-insert the
//! list with a dense `SortOrder`). The rewrite used to insert the rows one
//! awaited statement at a time on the single writer, so a 5,000-track playlist
//! cost 5,000 statements per edit (gl-1264). It now inserts the whole list in
//! one, and stores exactly what the per-row inserts stored.
//!
//! This binary records the statements one call runs ([`sql_statements`]).

mod sql_statements;

use std::sync::Arc;

use ferrofin_core::{
    FerrofinItemCountService, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinLibraryManager, FerrofinPeopleRepository, FerrofinPlaylistManager,
    FerrofinServerApplicationPaths, ItemTypeLookup,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::collections::PlaylistManager;
use sql_statements::{record_statements, statements_of};
use uuid::Uuid;

/// How many tracks the playlist holds: enough that one statement per entry
/// cannot pass for one per edit.
const TRACKS: usize = 50;

/// `LinkedChildType.Manual`, what an add stores.
const MANUAL: i64 = 0;

/// `LinkedChildType.Shortcut`: a link that is no manual entry, which an edit
/// keeps as it is and the caller's list leaves out.
const SHORTCUT: i64 = 1;

/// Inserts a minimal item of `kind`.
async fn seed_item(db: &Database, id: Uuid, kind: BaseItemKind) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name")
           VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, 0, ?2)"#,
    )
    .bind(guid_to_db(id))
    .bind(kind.stored_type_name())
    .execute(db.writer())
    .await
    .expect("insert item");
}

/// Stores one link as Jellyfin may have left it: at `order`, gaps allowed.
async fn seed_link(db: &Database, playlist: Uuid, order: i64, child: Uuid, child_type: i64) {
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1, ?2, ?3, ?4)"#,
    )
    .bind(guid_to_db(playlist))
    .bind(order)
    .bind(guid_to_db(child))
    .bind(child_type)
    .execute(db.writer())
    .await
    .expect("insert link");
}

/// The playlist's rows as stored, in order: `(SortOrder, ChildId, ChildType)`,
/// the id as its exact text. Both numbers must be stored as integers.
async fn stored(db: &Database, playlist: Uuid) -> Vec<(i64, String, i64)> {
    sqlx::query_as::<_, (i64, String, i64, String)>(
        r#"SELECT "SortOrder", "ChildId", "ChildType",
                  typeof("SortOrder") || ' ' || typeof("ChildType")
           FROM "LinkedChildren" WHERE "ParentId" = ?1 ORDER BY "SortOrder""#,
    )
    .bind(guid_to_db(playlist))
    .fetch_all(db.pool())
    .await
    .expect("entries")
    .into_iter()
    .map(|(order, child, child_type, types)| {
        assert_eq!(types, "integer integer", "SortOrder and ChildType storage");
        (order, child, child_type)
    })
    .collect()
}

/// `entries` as the rewrite stores them: `SortOrder` dense from 0 in that
/// order, each id in its canonical stored form.
fn dense(entries: &[(Uuid, i64)]) -> Vec<(i64, String, i64)> {
    entries
        .iter()
        .enumerate()
        .map(|(order, (id, child_type))| {
            (
                i64::try_from(order).expect("order"),
                guid_to_db(*id),
                *child_type,
            )
        })
        .collect()
}

/// Runs `call` and returns its output and how many `LinkedChildren` inserts
/// it ran.
async fn entry_inserts<T>(call: impl std::future::Future<Output = T>) -> (T, usize) {
    let (output, statements) = statements_of(call).await;
    let inserts = statements
        .iter()
        .filter(|statement| statement.contains(r#"INSERT INTO "LinkedChildren""#))
        .count();
    (output, inserts)
}

/// An adopted playlist (its ordinals 0, 2 and 5, a shortcut link among its
/// manual ones) takes an add, a move and a remove: each inserts its entries in
/// one statement, closes the gaps and keeps every entry's id and type.
#[tokio::test]
async fn a_playlist_edit_inserts_its_entries_in_one_statement() {
    record_statements();
    let root = tempfile::tempdir().expect("tempdir");
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let items = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let library = Arc::new(FerrofinLibraryManager::new(
        items.clone(),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        Arc::new(FerrofinItemPersistenceService::new(db.clone())),
        Arc::new(FerrofinPeopleRepository::new(db.clone())),
    ));
    let paths = Arc::new(FerrofinServerApplicationPaths::new(
        root.path().join("program"),
        root.path().join("log"),
        root.path().join("config"),
        root.path().join("cache"),
        root.path().join("web"),
    ));
    let manager = FerrofinPlaylistManager::new(db.clone(), library, items, paths);
    let playlist = Uuid::new_v4();
    seed_item(&db, playlist, BaseItemKind::Playlist).await;
    let tracks: Vec<Uuid> = (0..TRACKS).map(|_| Uuid::new_v4()).collect();
    for track in &tracks {
        seed_item(&db, *track, BaseItemKind::Audio).await;
    }
    seed_link(&db, playlist, 0, tracks[0], MANUAL).await;
    seed_link(&db, playlist, 2, tracks[1], SHORTCUT).await;
    seed_link(&db, playlist, 5, tracks[2], MANUAL).await;
    let mut expected = vec![
        (tracks[0], MANUAL),
        (tracks[1], SHORTCUT),
        (tracks[2], MANUAL),
    ];
    // An API key (the nil user) sees every manual entry.
    let caller = Uuid::nil();

    let added = &tracks[3..];
    let (outcome, inserts) =
        entry_inserts(manager.add_item_to_playlist(playlist, added, None, caller)).await;
    outcome.expect("add");
    assert_eq!(inserts, 1, "an add of {} tracks", added.len());
    expected.extend(added.iter().map(|id| (*id, MANUAL)));
    assert_eq!(stored(&db, playlist).await, dense(&expected));

    // Move the last track to the front: every entry is renumbered.
    let last = tracks[TRACKS - 1];
    let entry = last.simple().to_string();
    let (outcome, inserts) =
        entry_inserts(manager.move_item(&playlist.to_string(), &entry, 0, caller)).await;
    outcome.expect("move");
    assert_eq!(inserts, 1, "a move");
    expected.rotate_right(1);
    assert_eq!(stored(&db, playlist).await, dense(&expected));

    // Remove a manual entry: the rest close up behind it.
    let gone = tracks[2];
    let (outcome, inserts) = entry_inserts(
        manager.remove_item_from_playlist(&playlist.to_string(), &[gone.simple().to_string()]),
    )
    .await;
    outcome.expect("remove");
    assert_eq!(inserts, 1, "a remove");
    expected.retain(|(id, _)| *id != gone);
    assert_eq!(stored(&db, playlist).await, dense(&expected));
}
