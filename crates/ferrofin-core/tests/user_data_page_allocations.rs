//! A page's user data is resolved without copying what is already in hand.
//!
//! Every user-scoped item page reads the user's stored `UserData` rows for its
//! items and picks one row per item (`get_user_data_dtos_for_rows`, the port of
//! `GetUserDataBatch`). The picked row used to be cloned out of the rows read
//! and its key cloned again into the DTO, and every item's key list was derived
//! even for an item with exactly one stored row, which `ResolveUserDataRow`
//! picks whatever the keys are. An item with no row got an empty row with three
//! freshly formatted ids, only for its first key to be copied in. This binary
//! counts the heap allocations one page makes on its own thread, with its own
//! global allocator, and checks the per-item cost of each shape: no row, one
//! row, and several rows that the key order chooses between.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::stored_type_name;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinServerApplicationPaths,
    FerrofinServerConfigurationManager, FerrofinUserDataManager,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::UserItemDataDto;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::library::UserDataManager;
use ferrofin_traits::persistence::ItemPersistenceService as _;
use uuid::Uuid;

thread_local! {
    /// This thread's allocations (and reallocations) while it is counting,
    /// `None` while it is not. Per thread, so tests running side by side in
    /// one process (`cargo test`) never count each other's.
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The system allocator, counting what the current thread allocates while
/// it is counting. The database's worker threads are never counted.
struct Counting;

impl Counting {
    fn count() {
        // `try_with`: the slot is gone while the thread is being torn down.
        let _ = ALLOCATIONS.try_with(|count| {
            if let Some(n) = count.get() {
                count.set(Some(n + 1));
            }
        });
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counter is a
// const-initialised thread-local `Cell`, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Items on the page: one full read chunk of ids (`BATCH_BIND_CHUNK`).
const ITEMS: usize = 500;

/// The TMDB id every movie on the page carries, which leads its keys.
const TMDB: &str = "603";

/// The IMDb id every movie on the page carries, the second key.
const IMDB: &str = "tt0133093";

// The budgets, per item. The page's two reads (its `UserData` rows, then its
// provider ids for the keys) cost about 15 per item here, almost all of it
// binding and listing each id and decoding each provider row; each stored row
// read costs 3 more (its strings), plus 1 for the list an item's rows are read
// into, and an item's three keys and their list cost 4. Each budget is that
// sum plus 2 of slack. Copying the chosen row and its key, deriving the keys
// of a one-row item and building an unwritten item's empty row cost 9 more
// for a one-row item, 6 for an unwritten one and 5 for one with several rows.

/// Per item: the two reads.
const READS: usize = 15;

/// Per item: the list its stored rows are read into, when it has any.
const ROW_LIST: usize = 1;

/// Per stored row: its three strings.
const ROW: usize = 3;

/// Per item: its three keys and their list.
const KEYS: usize = 4;

/// Per item: what a later dependency may add to the reads.
const SLACK: usize = 2;

/// The most allocations per unwritten item: the reads, and its keys.
const UNWRITTEN_BUDGET: usize = READS + KEYS + SLACK;

/// The most allocations per item with one stored row: the reads, and its row.
const ONE_ROW_BUDGET: usize = READS + ROW_LIST + ROW + SLACK;

/// The most allocations per item with three stored rows: the reads, its rows,
/// and the keys that choose between them.
const SEVERAL_ROWS_BUDGET: usize = READS + ROW_LIST + 3 * ROW + KEYS + SLACK;

/// Inserts the minimal `Users` row the user-data foreign key needs.
async fn seed_user(db: &Database, id: Uuid) {
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'viewer', 'VIEWER')"#,
    )
    .bind(guid_to_db(id))
    .execute(db.writer())
    .await
    .expect("insert user");
}

/// Stores `count` movies, each with a TMDB and an IMDb id, and returns their
/// rows as a page query hands them over.
async fn seed_movies(db: &Database, count: usize) -> Vec<BaseItemEntity> {
    let rows: Vec<BaseItemEntity> = (0..count)
        .map(|n| BaseItemEntity {
            id: guid_to_db(Uuid::new_v4()),
            type_: stored_type_name(BaseItemKind::Movie)
                .expect("movie type")
                .to_owned(),
            name: Some(format!("Movie {n}")),
            path: Some(format!("/movies/Movie {n}.mkv")),
            ..BaseItemEntity::default()
        })
        .collect();
    let persistence = FerrofinItemPersistenceService::new(db.clone());
    persistence.save_items(&rows).await.expect("save movies");
    for row in &rows {
        let id = Uuid::parse_str(&row.id).expect("guid");
        for (provider, value) in [("Tmdb", TMDB), ("Imdb", IMDB)] {
            persistence
                .save_provider_id(id, provider, value)
                .await
                .expect("save provider id");
        }
    }
    rows
}

/// Stores one of `user`'s `UserData` rows on `item`, under `key`.
async fn seed_row(db: &Database, user: Uuid, item: &str, key: &str, play_count: i32) {
    sqlx::query(
        r#"INSERT INTO "UserData"
           ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
            "PlaybackPositionTicks", "Played")
           VALUES (?1, ?2, ?3, 0, ?4, 0, 0)"#,
    )
    .bind(item)
    .bind(guid_to_db(user))
    .bind(key)
    .bind(play_count)
    .execute(db.writer())
    .await
    .expect("insert user data");
}

/// A fresh database holding a user and a page of movies, the manager that
/// reads them, and the configuration's directory (kept for the test's life).
struct Page {
    db: Database,
    manager: FerrofinUserDataManager,
    user: Uuid,
    rows: Vec<BaseItemEntity>,
    _root: tempfile::TempDir,
}

async fn page() -> Page {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = Arc::new(FerrofinServerApplicationPaths::new(
        root.path().join("program"),
        root.path().join("log"),
        root.path().join("config"),
        root.path().join("cache"),
        root.path().join("web"),
    ));
    let config = Arc::new(
        FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("configuration"),
    );
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let user = Uuid::new_v4();
    seed_user(&db, user).await;
    let rows = seed_movies(&db, ITEMS).await;
    let manager =
        FerrofinUserDataManager::new(db.clone(), config as Arc<dyn ServerConfigurationManager>);
    Page {
        db,
        manager,
        user,
        rows,
        _root: root,
    }
}

/// Resolves the page's user data with provider ids hydrated, as `/Items/{id}`
/// and a `Fields=ProviderIds` page do, counting this thread's allocations.
async fn counted(page: &Page) -> (HashMap<Uuid, UserItemDataDto>, usize) {
    ALLOCATIONS.with(|count| count.set(Some(0)));
    let resolved = page
        .manager
        .get_user_data_dtos_for_rows(&page.rows, page.user, true)
        .await;
    let allocations = ALLOCATIONS.with(|count| count.replace(None)).unwrap_or(0);
    (resolved.expect("user data"), allocations)
}

/// The DTO resolved for `row`.
fn dto<'a>(map: &'a HashMap<Uuid, UserItemDataDto>, row: &BaseItemEntity) -> &'a UserItemDataDto {
    &map[&Uuid::parse_str(&row.id).expect("guid")]
}

/// Checks a page's count against `budget` allocations per item.
fn check(shape: &str, allocations: usize, budget: usize) {
    assert!(
        allocations <= budget * ITEMS,
        "resolving {ITEMS} items' user data ({shape}) made {allocations} allocations, \
         more than {budget} per item"
    );
}

/// An item the user never wrote reports its first key, TMDB. Its keys are
/// derived, the first moving into the DTO, and no empty row is built.
#[tokio::test(flavor = "current_thread")]
async fn a_page_of_unwritten_items_builds_only_their_keys() {
    let page = page().await;

    let (map, allocations) = counted(&page).await;

    assert_eq!(map.len(), ITEMS);
    for row in &page.rows {
        let dto = dto(&map, row);
        assert_eq!(dto.key, TMDB);
        assert_eq!(dto.play_count, 0);
        assert!(!dto.played && !dto.is_favorite);
    }
    check("unwritten", allocations, UNWRITTEN_BUDGET);
}

/// An item with exactly one stored row is answered with that row, under its
/// own key, whatever the item's keys are, so none is derived and the row
/// moves into the DTO.
#[tokio::test(flavor = "current_thread")]
async fn a_page_of_items_with_one_row_derives_no_keys() {
    let page = page().await;
    for row in &page.rows {
        seed_row(&page.db, page.user, &row.id, "legacy-key", 4).await;
    }

    let (map, allocations) = counted(&page).await;

    assert_eq!(map.len(), ITEMS);
    for row in &page.rows {
        let dto = dto(&map, row);
        assert_eq!(dto.key, "legacy-key", "the only row, matched by no key");
        assert_eq!(dto.play_count, 4);
    }
    check("one row", allocations, ONE_ROW_BUDGET);
}

/// Several stored rows are chosen between by the keys, TMDB first, although
/// the IMDb row sorts first; the chosen row moves out of the rows read.
#[tokio::test(flavor = "current_thread")]
async fn a_page_of_items_with_several_rows_moves_the_preferred_one() {
    let page = page().await;
    for row in &page.rows {
        seed_row(&page.db, page.user, &row.id, IMDB, 3).await;
        seed_row(&page.db, page.user, &row.id, TMDB, 7).await;
        seed_row(&page.db, page.user, &row.id, &row.id.to_lowercase(), 1).await;
    }

    let (map, allocations) = counted(&page).await;

    assert_eq!(map.len(), ITEMS);
    for row in &page.rows {
        let dto = dto(&map, row);
        assert_eq!(dto.key, TMDB, "the TMDB row, which leads the keys");
        assert_eq!(dto.play_count, 7);
    }
    check("several rows", allocations, SEVERAL_ROWS_BUDGET);
}
