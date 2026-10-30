//! One item's failure stays that item's, as upstream's per-child `catch` in
//! `LimitedConcurrencyLibraryScheduler.ProcessItem` keeps it: the rest of the
//! walk, the prune, the closing passes and the `LibraryChanged` push still run.
//!
//! - a Series or Season row the decoder rejects: its children read it as their
//!   parent, and the read fails on every scan, so the scan must not depend on
//!   it: a child keeps its own values (`FindSeriesName` for a parent that is
//!   not there);
//! - a write that fails for one item, before or after its row is saved: the
//!   item is skipped and the next scan refreshes it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferrofin_core::event_manager::consumer_done;
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::item_visibility::ItemVisibility;
use ferrofin_core::library_changed_notifier::LibraryChangedNotifier;
use ferrofin_core::{
    FerrofinEventManager, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinVirtualFolderManager, LibraryScanner, LocalizationManager, ScanOutcome,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::events::EventManager;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;
use rstest::rstest;

/// A shows library and a movies library on one database, scanned the way the
/// server wires it: with the item repository and the hierarchy-aware rating
/// resolver, whose reads an undecodable parent fails.
struct Fixture {
    _tmp: tempfile::TempDir,
    db: Database,
    shows: PathBuf,
    movies: PathBuf,
    scanner: LibraryScanner,
    notifier: Arc<LibraryChangedNotifier>,
    changes: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let shows = tmp.path().join("shows");
        let movies = tmp.path().join("movies");
        std::fs::create_dir_all(&shows).expect("shows");
        std::fs::create_dir_all(&movies).expect("movies");
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        for (name, kind, path) in [
            ("Shows", CollectionTypeOptions::tvshows, &shows),
            ("Movies", CollectionTypeOptions::movies, &movies),
        ] {
            vf.add_virtual_folder(
                name,
                Some(kind),
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: path.to_string_lossy().into_owned(),
                    }],
                    ..LibraryOptions::default()
                },
            )
            .await
            .expect("add library");
        }
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let visibility = Arc::new(ItemVisibility::new(
            db.clone(),
            Arc::clone(&items),
            Arc::new(LocalizationManager::new("US")),
            tmp.path().join("data").to_string_lossy().into_owned(),
        ));
        let events = FerrofinEventManager::new();
        let changes: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&changes);
        events.subscribe(
            "LibraryChanged",
            Arc::new(move |payload: &str| {
                sink.lock()
                    .expect("sink")
                    .push(serde_json::from_str(payload).expect("json"));
                consumer_done()
            }),
        );
        let events = Arc::new(events);
        let notifier = Arc::new(LibraryChangedNotifier::new(
            Arc::clone(&events) as Arc<dyn EventManager>,
            std::time::Duration::from_secs(3600),
        ));
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_items(items)
            .with_localization(Arc::new(LocalizationManager::new("US")))
            .with_rating_resolver(visibility)
            .with_events(events as Arc<dyn EventManager>)
            .with_change_notifier(Arc::clone(&notifier));
        Self {
            _tmp: tmp,
            db,
            shows,
            movies,
            scanner,
            notifier,
            changes,
        }
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }

    fn movie(&self, name: &str) -> PathBuf {
        let path = self.movies.join(format!("{name}.mkv"));
        Self::write(&path, "");
        path
    }

    fn episode(&self, show: &str, number: u32) -> PathBuf {
        let path = self.episode_path(show, number);
        Self::write(&path, "");
        path
    }

    fn episode_path(&self, show: &str, number: u32) -> PathBuf {
        self.shows
            .join(show)
            .join("Season 1")
            .join(format!("{show} S01E{number:02}.mkv"))
    }

    /// Every id announced as added since the last call.
    async fn added(&self) -> Vec<String> {
        self.notifier.flush().await;
        self.changes
            .lock()
            .expect("changes")
            .drain(..)
            .flat_map(|c| c["ItemsAdded"].as_array().cloned().unwrap_or_default())
            .map(|v| v.as_str().unwrap_or_default().to_owned())
            .collect()
    }

    async fn run(&self, sql: &str) {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(self.db.pool())
            .await
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }

    /// A stored value the row decoder rejects, on the `kind` row of the
    /// series `show`. The scan upsert keeps a stored `DateCreated`, so it
    /// stays so.
    async fn corrupt(&self, kind: &str, show: &str) {
        let column = if kind == "Series" {
            "Name"
        } else {
            "SeriesName"
        };
        self.run(&format!(
            r#"UPDATE "BaseItems" SET "DateCreated" = 'not a date'
               WHERE "Type" LIKE '%.{kind}' AND "{column}" = '{show}'"#
        ))
        .await;
    }

    async fn refreshed(&self, path: &Path) -> Option<String> {
        sqlx::query_scalar(r#"SELECT "DateLastRefreshed" FROM "BaseItems" WHERE "Path" = ?1"#)
            .bind(path.to_string_lossy().into_owned())
            .fetch_one(self.db.pool())
            .await
            .expect("stamp")
    }

    async fn rating_score(&self, path: &Path) -> Option<i64> {
        sqlx::query_scalar(
            r#"SELECT "InheritedParentalRatingValue" FROM "BaseItems" WHERE "Path" = ?1"#,
        )
        .bind(path.to_string_lossy().into_owned())
        .fetch_one(self.db.pool())
        .await
        .expect("score")
    }

    /// `(Name, SeriesName, SeasonName)` of the row at `path`, without decoding
    /// the rest of it.
    async fn names(&self, path: &Path) -> Option<(String, Option<String>, Option<String>)> {
        sqlx::query_as(
            r#"SELECT "Name", "SeriesName", "SeasonName" FROM "BaseItems" WHERE "Path" = ?1"#,
        )
        .bind(path.to_string_lossy().into_owned())
        .fetch_optional(self.db.pool())
        .await
        .expect("names")
    }
}

fn movie_id(path: &Path) -> String {
    derive_item_id(BaseItemKind::Movie, &path.to_string_lossy())
        .expect("movie id")
        .simple()
        .to_string()
}

// A Series or Season row the decoder rejects, followed by a new episode under
// it, a healthy series, a new movie and a deleted one in the other library.
// The row's children read it as their parent on every scan, so before the fix
// each scan ended there: nothing later was refreshed, and nothing was pruned.
// `FindSeriesName` answers the child's own name when the parent is not there.
#[rstest]
#[case::series("Series")]
#[case::season("Season")]
#[tokio::test]
async fn an_undecodable_parent_stops_nothing_else(#[case] kind: &str) {
    let fx = Fixture::new().await;
    let existing = fx.episode("Corrupt", 1);
    fx.episode("Healthy", 1);
    let doomed = fx.movie("Doomed");
    fx.movie("Kept");
    fx.scanner.scan_all().await.expect("first scan");
    fx.added().await;

    fx.corrupt(kind, "Corrupt").await;
    let new_episode = fx.episode("Corrupt", 2);
    let later_episode = fx.episode("Healthy", 2);
    let new_movie = fx.movie("Newcomer");
    std::fs::remove_file(&doomed).expect("delete");

    let rescan = fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.skipped, 0, "a child of the row is refreshed");
    assert_eq!(rescan.removed, 1, "the deleted movie is pruned");
    assert!(fx.names(&doomed).await.is_none());
    assert!(fx.names(&new_movie).await.is_some(), "the later library");
    assert!(fx.names(&later_episode).await.is_some(), "the later series");
    let (episode, series, season) = fx.names(&new_episode).await.expect("the new episode");
    assert_eq!(episode, "Corrupt S01E02");
    assert_eq!(series.as_deref(), Some("Corrupt"), "its own name stays");
    assert_eq!(season.as_deref(), Some("Season 1"), "its own name stays");
    assert_eq!(
        fx.added().await.len(),
        3,
        "the push still announces what the scan added"
    );

    // The row stays bad, and the same scan keeps working.
    fx.scanner.scan_all().await.expect("scan after that");
    assert_eq!(
        fx.names(&existing).await.expect("existing").1.as_deref(),
        Some("Corrupt")
    );
}

// The key a series settled on comes from its provider ids, which a series the
// scan could not refresh does not settle: its children keep the key the row
// holds, not the plan's, or they would leave their series' Next Up and played
// counts for as long as the row stays bad.
#[tokio::test]
async fn children_of_an_undecodable_series_keep_its_stored_key() {
    let fx = Fixture::new().await;
    let episode = fx.episode("Corrupt", 1);
    fx.scanner.scan_all().await.expect("first scan");
    fx.run(
        r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = 'settled-key'
           WHERE "Type" LIKE '%.Series'"#,
    )
    .await;
    fx.run(
        r#"UPDATE "BaseItems" SET "SeriesPresentationUniqueKey" = 'settled-key'
           WHERE "Type" LIKE '%.Season' OR "Type" LIKE '%.Episode'"#,
    )
    .await;
    fx.corrupt("Series", "Corrupt").await;
    let new_episode = fx.episode("Corrupt", 2);

    fx.scanner.scan_all().await.expect("rescan");

    for path in [&episode, &new_episode] {
        let key: Option<String> = sqlx::query_scalar(
            r#"SELECT "SeriesPresentationUniqueKey" FROM "BaseItems" WHERE "Path" = ?1"#,
        )
        .bind(path.to_string_lossy().into_owned())
        .fetch_one(fx.db.pool())
        .await
        .expect("key");
        assert_eq!(key.as_deref(), Some("settled-key"), "{}", path.display());
    }
}

// A season's name goes with its row, so its episodes keep the one they have.
#[tokio::test]
async fn an_episode_of_an_undecodable_season_keeps_its_season_name() {
    let fx = Fixture::new().await;
    let episode = fx.episode("Show", 1);
    fx.scanner.scan_all().await.expect("first scan");
    let before = fx.names(&episode).await.expect("episode").2;
    fx.corrupt("Season", "Show").await;
    fx.episode("Show", 2);

    fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(fx.names(&episode).await.expect("episode").2, before);
}

// A child's rating score reads its whole parent chain, which an undecodable
// parent fails: the child is still scored, from its own rating.
#[tokio::test]
async fn a_child_of_an_undecodable_series_is_still_rated() {
    let fx = Fixture::new().await;
    fx.episode("Corrupt", 1);
    fx.scanner.scan_all().await.expect("first scan");
    fx.corrupt("Series", "Corrupt").await;
    let new_episode = fx.episode("Corrupt", 2);
    Fixture::write(
        &new_episode.with_extension("nfo"),
        "<episodedetails><title>Rated</title><mpaa>TV-MA</mpaa></episodedetails>",
    );

    fx.scanner.scan_all().await.expect("rescan");

    let (rating, score): (Option<String>, Option<i64>) = sqlx::query_as(
        r#"SELECT "OfficialRating", "InheritedParentalRatingValue" FROM "BaseItems"
           WHERE "Path" = ?1"#,
    )
    .bind(new_episode.to_string_lossy().into_owned())
    .fetch_one(fx.db.pool())
    .await
    .expect("row");
    assert_eq!(rating.as_deref(), Some("TV-MA"));
    assert_eq!(
        score,
        LocalizationManager::new("US")
            .get_rating_score("TV-MA", None)
            .map(|score| i64::from(score.score))
    );
}

// Stored and new children with no rating of their own inherit their series'.
// The chain that scores them cannot be read, and a bad row must not lift a
// restriction: a NULL score is an unrated item to the max-rating filters.
#[tokio::test]
async fn a_child_of_an_undecodable_series_keeps_the_inherited_rating() {
    let fx = Fixture::new().await;
    let existing = fx.episode("Corrupt", 1);
    Fixture::write(
        &fx.shows.join("Corrupt").join("tvshow.nfo"),
        "<tvshow><title>Corrupt</title><mpaa>TV-MA</mpaa></tvshow>",
    );
    fx.scanner.scan_all().await.expect("first scan");
    let score = LocalizationManager::new("US")
        .get_rating_score("TV-MA", None)
        .map(|score| i64::from(score.score));
    assert!(score.is_some());
    assert_eq!(
        fx.rating_score(&existing).await,
        score,
        "the setup inherits"
    );
    fx.corrupt("Series", "Corrupt").await;
    let new_episode = fx.episode("Corrupt", 2);
    // Refreshed anew, so the stored child is scored again too.
    std::fs::write(&existing, b"grown").expect("grow");

    fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(fx.rating_score(&existing).await, score, "the stored child");
    assert_eq!(fx.rating_score(&new_episode).await, score, "the new child");
}

// A loose episode's `ParentId` is the series, but it inherits through its
// season, which may carry a rating of its own.
#[tokio::test]
async fn a_loose_episode_inherits_through_its_season() {
    let fx = Fixture::new().await;
    let show = fx.shows.join("Corrupt");
    Fixture::write(&show.join("Corrupt S01E01.mkv"), "");
    fx.scanner.scan_all().await.expect("first scan");
    let score = LocalizationManager::new("US")
        .get_rating_score("TV-MA", None)
        .map(|score| i64::from(score.score));
    fx.run(
        r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = 5
           WHERE "Type" LIKE '%.Series'"#,
    )
    .await;
    fx.run(&format!(
        r#"UPDATE "BaseItems" SET "OfficialRating" = 'TV-MA',
               "InheritedParentalRatingValue" = {}
           WHERE "Type" LIKE '%.Season'"#,
        score.expect("score")
    ))
    .await;
    fx.corrupt("Series", "Corrupt").await;
    let new_episode = show.join("Corrupt S01E02.mkv");
    Fixture::write(&new_episode, "");

    fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(fx.rating_score(&new_episode).await, score);
}

// A watcher's path-scoped scan takes the same road.
#[tokio::test]
async fn a_scoped_scan_refreshes_a_child_of_an_undecodable_series() {
    let fx = Fixture::new().await;
    fx.episode("Corrupt", 1);
    fx.scanner.scan_all().await.expect("first scan");
    fx.corrupt("Series", "Corrupt").await;
    let new_episode = fx.episode("Corrupt", 2);

    fx.scanner
        .scan_paths(&[new_episode.to_string_lossy().into_owned()])
        .await
        .expect("scoped scan");

    assert!(fx.names(&new_episode).await.is_some());
}

// Every item's rating score reads its library's row.
#[tokio::test]
async fn an_undecodable_library_row_stops_nothing_else() {
    let fx = Fixture::new().await;
    fx.episode("Show", 1);
    let doomed = fx.movie("Doomed");
    fx.scanner.scan_all().await.expect("first scan");
    fx.run(
        r#"UPDATE "BaseItems" SET "DateCreated" = 'not a date'
           WHERE "Type" LIKE '%.CollectionFolder' AND "Name" = 'Shows'"#,
    )
    .await;
    let new_episode = fx.episode("Show", 2);
    let new_movie = fx.movie("Newcomer");
    std::fs::remove_file(&doomed).expect("delete");

    let rescan = fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.removed, 1);
    assert!(fx.names(&new_episode).await.is_some());
    assert!(fx.names(&new_movie).await.is_some());
}

/// The messages of the warnings emitted while it is the default subscriber.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Message<'a>(&'a mut String);
        impl tracing::field::Visit for Message<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    *self.0 = format!("{value:?}");
                }
            }
        }
        if *event.metadata().level() == tracing::Level::WARN {
            let mut message = String::new();
            event.record(&mut Message(&mut message));
            self.0.lock().expect("warnings").push(message);
        }
    }
}

// Every child of the row misses the name cache, so without a remembered miss
// each one would read the row and warn about it again.
#[tokio::test]
async fn an_undecodable_parent_is_asked_for_once_per_scan() {
    use tracing_subscriber::layer::SubscriberExt as _;
    let fx = Fixture::new().await;
    fx.episode("Corrupt", 1);
    fx.episode("Corrupt", 2);
    fx.episode("Corrupt", 3);
    fx.scanner.scan_all().await.expect("first scan");
    fx.corrupt("Series", "Corrupt").await;

    let warnings = Warnings::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(warnings.clone()));
    fx.scanner.scan_all().await.expect("rescan");

    let warned = warnings.0.lock().expect("warnings").clone();
    assert_eq!(
        warned
            .iter()
            .filter(|message| {
                message.contains("could not read a parent row")
                    || message.contains("could not read series display order")
            })
            .count(),
        1,
        "{warned:?}"
    );
}

// A write that fails for one new item skips that item alone. It is neither
// counted as created nor announced, it stays out of the prune, and the next
// scan creates it.
#[tokio::test]
async fn a_failed_save_skips_only_that_item() {
    let fx = Fixture::new().await;
    let doomed = fx.movie("Doomed");
    fx.scanner.scan_all().await.expect("first scan");
    fx.added().await;
    fx.run(
        r#"CREATE TRIGGER "refuse_broken" BEFORE INSERT ON "BaseItems"
           WHEN NEW."Name" = 'Broken'
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#,
    )
    .await;
    let broken = fx.movie("Broken");
    let later = fx.movie("Later");
    std::fs::remove_file(&doomed).expect("delete");

    let rescan = fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(
        rescan,
        ScanOutcome {
            created: 1,
            removed: 1,
            skipped: 1,
            ..ScanOutcome::default()
        }
    );
    assert!(fx.names(&broken).await.is_none());
    assert!(fx.names(&later).await.is_some(), "the item after it");
    assert!(fx.names(&doomed).await.is_none(), "the prune ran");
    assert_eq!(fx.added().await, vec![movie_id(&later)]);

    fx.run(r#"DROP TRIGGER "refuse_broken""#).await;
    let retry = fx.scanner.scan_all().await.expect("retry");
    assert_eq!(retry.created, 1, "the next scan creates it");
    assert_eq!(retry.skipped, 0);
    assert_eq!(fx.added().await, vec![movie_id(&broken)]);
}

// A failure after the row was saved must not leave it stamped as refreshed: a
// stamped row whose file did not change is not looked at again, so whatever
// the failed step owed would stay lost. Here the ancestor closure, written
// after the row: the stamp goes, and the next scan finishes the item. A row
// that was saved exists, so a new one is still announced.
#[rstest]
#[case::new_item(true)]
#[case::stored_item(false)]
#[tokio::test]
async fn a_failure_after_the_row_is_saved_leaves_the_item_to_retry(#[case] new: bool) {
    let fx = Fixture::new().await;
    let stored = fx.movie("Stored");
    fx.scanner.scan_all().await.expect("first scan");
    fx.added().await;
    let failing = if new {
        fx.movie("Fresh")
    } else {
        stored.clone()
    };
    if !new {
        // A lost closure: the scan saves the row, then writes the closure.
        fx.run(r#"DELETE FROM "AncestorIds" WHERE "ItemId" IN (SELECT "Id" FROM "BaseItems" WHERE "Name" = 'Stored')"#)
            .await;
        assert!(
            fx.refreshed(&stored).await.is_some(),
            "the row starts stamped"
        );
    }
    fx.run(&format!(
        r#"CREATE TRIGGER "refuse_closure" BEFORE INSERT ON "AncestorIds"
           WHEN NEW."ItemId" = (SELECT "Id" FROM "BaseItems" WHERE "Path" = '{}')
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#,
        failing.to_string_lossy()
    ))
    .await;

    let rescan = fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.skipped, 1);
    assert_eq!(rescan.created, 0);
    assert!(fx.names(&failing).await.is_some(), "the row was saved");
    assert_eq!(fx.refreshed(&failing).await, None, "no stamp");
    assert_eq!(
        fx.added().await,
        if new {
            vec![movie_id(&failing)]
        } else {
            Vec::new()
        }
    );

    fx.run(r#"DROP TRIGGER "refuse_closure""#).await;
    let retry = fx.scanner.scan_all().await.expect("retry");
    assert_eq!(retry.skipped, 0);
    assert_eq!(retry.updated, 1, "the next scan finishes it");
    assert!(fx.refreshed(&failing).await.is_some(), "stamped now");
    let closure: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "AncestorIds" WHERE "ItemId" =
           (SELECT "Id" FROM "BaseItems" WHERE "Path" = ?1)"#,
    )
    .bind(failing.to_string_lossy().into_owned())
    .fetch_one(fx.db.pool())
    .await
    .expect("closure");
    assert!(closure > 0, "and writes what it owed");
}

// A series whose save fails has settled on a key its row does not hold, so its
// children stay on the key the row does hold.
#[tokio::test]
async fn children_of_a_series_that_failed_to_save_keep_its_stored_key() {
    let fx = Fixture::new().await;
    fx.episode("Show", 1);
    fx.scanner.scan_all().await.expect("first scan");
    fx.run(
        r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = 'settled-key',
               "DateLastRefreshed" = NULL
           WHERE "Type" LIKE '%.Series'"#,
    )
    .await;
    fx.run(
        r#"UPDATE "BaseItems" SET "SeriesPresentationUniqueKey" = 'settled-key'
           WHERE "Type" LIKE '%.Season' OR "Type" LIKE '%.Episode'"#,
    )
    .await;
    fx.run(
        r#"CREATE TRIGGER "refuse_series" BEFORE UPDATE ON "BaseItems"
           WHEN OLD."Type" LIKE '%.Series'
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#,
    )
    .await;
    let new_episode = fx.episode("Show", 2);

    let rescan = fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.skipped, 1);
    let key: Option<String> = sqlx::query_scalar(
        r#"SELECT "SeriesPresentationUniqueKey" FROM "BaseItems" WHERE "Path" = ?1"#,
    )
    .bind(new_episode.to_string_lossy().into_owned())
    .fetch_one(fx.db.pool())
    .await
    .expect("key");
    assert_eq!(key.as_deref(), Some("settled-key"));
}

// A series' save writes its row and then pushes its name to its children. When
// the push fails the row already carries this refresh's stamp and file facts,
// so the stamp goes even though the save itself reported the failure.
#[tokio::test]
async fn a_save_that_fails_after_writing_the_row_leaves_it_to_retry() {
    let fx = Fixture::new().await;
    fx.episode("Show", 1);
    let series = fx.shows.join("Show");
    fx.scanner.scan_all().await.expect("first scan");
    fx.run(r#"UPDATE "BaseItems" SET "DateLastRefreshed" = NULL WHERE "Type" LIKE '%.Series'"#)
        .await;
    fx.run(r#"UPDATE "BaseItems" SET "SeriesName" = 'Stale' WHERE "Type" LIKE '%.Season'"#)
        .await;
    fx.run(
        r#"CREATE TRIGGER "refuse_name" BEFORE UPDATE OF "SeriesName" ON "BaseItems"
           WHEN OLD."Type" LIKE '%.Season'
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#,
    )
    .await;

    let rescan = fx.scanner.scan_all().await.expect("rescan");

    assert!(rescan.skipped > 0);
    assert_eq!(fx.refreshed(&series).await, None, "no stamp");

    fx.run(r#"DROP TRIGGER "refuse_name""#).await;
    let retry = fx.scanner.scan_all().await.expect("retry");
    assert_eq!(retry.skipped, 0);
    assert!(fx.refreshed(&series).await.is_some(), "stamped now");
}

// A scan that skipped an item did not write its row, so no library counts as
// gone over by this build, which is what lets a delete remove files there.
#[tokio::test]
async fn a_scan_that_skipped_an_item_does_not_record_the_library() {
    let fx = Fixture::new().await;
    fx.run(
        r#"CREATE TRIGGER "refuse_broken" BEFORE INSERT ON "BaseItems"
           WHEN NEW."Name" = 'Broken'
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#,
    )
    .await;
    fx.movie("Broken");
    let recorded = || async {
        sqlx::query_scalar::<_, i64>(
            r#"SELECT COUNT(*) FROM "FerrofinMeta" WHERE "Key" LIKE 'library_scanned_by:%'"#,
        )
        .fetch_one(fx.db.pool())
        .await
        .expect("recorded")
    };

    let scan = fx.scanner.scan_all().await.expect("scan");

    assert_eq!(scan.skipped, 1);
    assert_eq!(recorded().await, 0);

    fx.run(r#"DROP TRIGGER "refuse_broken""#).await;
    fx.scanner.scan_all().await.expect("retry");
    assert_eq!(recorded().await, 2, "both libraries");
}

// A new series that cannot be saved leaves nothing for its seasons and episodes
// to hang off: `RefreshChildMetadata` does not recurse into it, and a child's
// ancestor closure would fail on its foreign key after the child's row was
// saved. They wait for the next scan, with the rest of the walk going on.
#[tokio::test]
async fn a_new_series_that_fails_takes_its_children_along() {
    let fx = Fixture::new().await;
    fx.scanner.scan_all().await.expect("first scan");
    fx.run(
        r#"CREATE TRIGGER "refuse_broken" BEFORE INSERT ON "BaseItems"
           WHEN NEW."Name" = 'Broken'
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#,
    )
    .await;
    let child = fx.episode("Broken", 1);
    let later = fx.episode("Later", 1);

    let rescan = fx.scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.skipped, 3, "the series, its season and its episode");
    assert!(fx.names(&child).await.is_none(), "no orphan");
    assert!(fx.names(&later).await.is_some(), "the walk went on");

    fx.run(r#"DROP TRIGGER "refuse_broken""#).await;
    let retry = fx.scanner.scan_all().await.expect("retry");
    assert_eq!(retry.created, 3);
    assert_eq!(retry.skipped, 0);
    assert!(fx.names(&child).await.is_some());
}

// Identify's Apply awaits the refresh of one item, and upstream's
// `item.RefreshMetadata` throws to it: only `Folder.RunTasks`' children are
// caught. A refresh that applied nothing must not report success.
#[tokio::test]
async fn an_item_refresh_ends_with_its_items_failure() {
    use ferrofin_core::{ScanCancel, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
    let fx = Fixture::new().await;
    let movie = fx.movie("Stored");
    fx.scanner.scan_all().await.expect("first scan");
    let options = MetadataRefreshOptions {
        replace_all_metadata: true,
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    fx.run(
        r#"CREATE TRIGGER "refuse_stored" BEFORE UPDATE OF "Name" ON "BaseItems"
           WHEN OLD."Name" = 'Stored'
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#,
    )
    .await;

    let refreshed = fx
        .scanner
        .scan_target(
            &ScanTarget::Items(vec![movie.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &none, &ScanCancel::new())
                .with_passes(ferrofin_core::ScanPasses::Touched),
        )
        .await;

    assert!(refreshed.is_err(), "{refreshed:?}");

    // A folder's refresh of the same item goes through `Folder.RunTasks`.
    let folder = fx
        .scanner
        .scan_target(
            &ScanTarget::Paths(vec![fx.movies.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &none, &ScanCancel::new())
                .with_passes(ferrofin_core::ScanPasses::Touched),
        )
        .await
        .expect("folder refresh");
    assert_eq!(folder.skipped, 1);

    // The refresh of a new item whose new parent cannot be saved applied
    // nothing either, though the item itself never ran.
    fx.run(
        r#"CREATE TRIGGER "refuse_series" BEFORE INSERT ON "BaseItems"
           WHEN NEW."Type" LIKE '%.Series'
           BEGIN SELECT RAISE(ABORT, 'refused'); END"#,
    )
    .await;
    let episode = fx.episode("New", 1);
    let refreshed = fx
        .scanner
        .scan_target(
            &ScanTarget::Items(vec![episode.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &none, &ScanCancel::new())
                .with_passes(ferrofin_core::ScanPasses::Touched),
        )
        .await;
    assert!(refreshed.is_err(), "{refreshed:?}");
    assert!(fx.names(&episode).await.is_none());
}
