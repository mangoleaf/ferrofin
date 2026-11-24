//! `ferrofin_db::export::export_data_dir`, the copy of a stopped data directory
//! that `ferrofin-server export-jellyfin` writes (gl-1548): the tree copied,
//! the database snapshot holding what the write-ahead log held, the source
//! left as it was, and a source that changes during the export refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ferrofin_db::export::{
    CarriedRows, ContainerFile, DOCKER, DatabaseEdits, DroppedObjects, DroppedTable,
    EXPORTED_DATABASE, HistoryRewrite, IdentityLink, LeftOut, LibraryContainer, MirroredLiveTv,
    PathRewrite, RecordingRow, SeriesTimerRow, SourceDirs, TimerRow, export_data_dir,
};
use ferrofin_db::{Database, DbError};
use sqlx::ConnectOptions as _;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Every file under `root`, relative to it, with its bytes.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                found.insert(path.strip_prefix(root).unwrap().to_owned(), bytes);
            }
        }
    }
    found
}

/// Writes a SQLite database to `database` (and its `-wal`) as a killed server
/// leaves it: three committed rows, all of them still in the write-ahead log.
async fn database_with_unfolded_log(scratch: &Path, database: &Path) {
    let live = scratch.join("live.db");
    let mut conn = SqliteConnectOptions::new()
        .filename(&live)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .pragma("wal_autocheckpoint", "0")
        .connect()
        .await
        .unwrap();
    // Ferrofin's own bookkeeping table: what makes it a Ferrofin database.
    sqlx::query("CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE Items (Name TEXT NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    for name in ["one", "two", "three"] {
        sqlx::query("INSERT INTO Items (Name) VALUES (?)")
            .bind(name)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    // Copied while the connection holds the log open, so the log is not folded in.
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    std::fs::copy(&live, database).unwrap();
    std::fs::copy(with_suffix(&live, "-wal"), with_suffix(database, "-wal")).unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    assert!(
        std::fs::metadata(with_suffix(database, "-wal"))
            .unwrap()
            .len()
            > 0
    );
}

/// A data directory in Jellyfin's layout, its database's commits in the log, a
/// stale `-shm` beside it, and some artwork and configuration.
async fn source(scratch: &Path) -> PathBuf {
    let source = scratch.join("source");
    database_with_unfolded_log(scratch, &source.join(EXPORTED_DATABASE)).await;
    std::fs::write(
        with_suffix(&source.join(EXPORTED_DATABASE), "-shm"),
        [7_u8; 64],
    )
    .unwrap();
    std::fs::create_dir_all(source.join("metadata/library/ab/ab12")).unwrap();
    std::fs::write(
        source.join("metadata/library/ab/ab12/poster.jpg"),
        b"jpeg bytes",
    )
    .unwrap();
    std::fs::create_dir_all(source.join("config")).unwrap();
    std::fs::write(source.join("config/system.json"), b"{}").unwrap();
    source
}

/// The `Name` of every row in the exported database's `Items`.
async fn exported_names(out: &Path) -> Vec<String> {
    let mut conn = SqliteConnectOptions::new()
        .filename(out.join(EXPORTED_DATABASE))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let names = sqlx::query_scalar("SELECT Name FROM Items ORDER BY rowid")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    names
}

#[tokio::test]
async fn the_export_copies_the_tree_and_folds_the_log_into_the_snapshot() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    // Jellyfin's pre-10.11 migration list, which Jellyfin 12.4 would read
    // again, and the copy Jellyfin 10.11 renamed it to, which it never reads.
    let listing = b"<MigrationOptions><Applied><ValueTupleOfGuidString><Item1>9b354818-94d5-4b68-ac49-e35cb85f9d84</Item1><Item2>CreateNetworkConfiguration</Item2></ValueTupleOfGuidString></Applied></MigrationOptions>";
    for name in ["migrations.xml", "migrations.xml.backup"] {
        std::fs::write(source.join("config").join(name), listing).unwrap();
    }
    let out = scratch.path().join("out");
    let before = files(&source);

    let summary = export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join(EXPORTED_DATABASE),
        &out,
    )
    .await
    .unwrap();

    assert_eq!(
        files(&source),
        before,
        "the source is left byte for byte as it was"
    );
    let copied = files(&out);
    assert_eq!(
        copied[Path::new("metadata/library/ab/ab12/poster.jpg")],
        b"jpeg bytes"
    );
    assert_eq!(copied[Path::new("config/system.json")], b"{}");
    assert!(!copied.contains_key(Path::new("config/migrations.xml")));
    assert!(copied.contains_key(Path::new("config/migrations.xml.backup")));
    // The database is the snapshot alone: none of the source's companion files.
    assert_eq!(
        copied
            .keys()
            .filter(|path| path.starts_with("data"))
            .collect::<Vec<_>>(),
        [Path::new(EXPORTED_DATABASE)]
    );
    assert_eq!(summary.files, 3);
    assert_eq!(summary.database, out.join(EXPORTED_DATABASE));
    // A native database has no Jellyfin history: it is given a fresh
    // install's (tests/export_history.rs checks its ids); Ferrofin's own
    // table goes.
    assert_eq!(summary.edits.history.recorded.len(), 105);
    assert_eq!(
        summary.edits,
        DatabaseEdits {
            history: HistoryRewrite {
                generation: None,
                recorded: summary.edits.history.recorded.clone(),
                left_to_run: Vec::new(),
            },
            dropped: DroppedObjects {
                tables: vec![DroppedTable {
                    name: "_sqlx_migrations".to_owned(),
                    rows: 0
                }],
                indexes: Vec::new(),
            },
            paths: PathRewrite {
                moves: summary.edits.paths.moves.clone(),
                ..PathRewrite::default()
            },
            // (the moves themselves are checked below)
            carried: CarriedRows::default(),
        }
    );
    assert_eq!(summary.left_out.len(), 0);
    // The source's own directory moves to the layout's, for the paths written
    // into files.
    assert_eq!(
        summary
            .edits
            .paths
            .moved(&format!("{}/data/livetv/x.ts", source.display())),
        Some("/config/data/livetv/x.ts".to_owned())
    );
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);
}

#[tokio::test]
async fn two_exports_of_one_source_hold_the_same_database() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let database = source.join(EXPORTED_DATABASE);
    let (first, second) = (scratch.path().join("first"), scratch.path().join("second"));

    export_data_dir(&SourceDirs::new(&source), &DOCKER, &database, &first)
        .await
        .unwrap();
    export_data_dir(&SourceDirs::new(&source), &DOCKER, &database, &second)
        .await
        .unwrap();

    assert_eq!(files(&first), files(&second));
}

#[tokio::test]
async fn a_native_database_is_exported_in_place_of_a_leftover_jellyfin_one() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("source");
    database_with_unfolded_log(scratch.path(), &source.join("ferrofin.db")).await;
    // An older database at Jellyfin's place, with a log of its own.
    std::fs::create_dir_all(source.join("data")).unwrap();
    std::fs::write(source.join(EXPORTED_DATABASE), b"not this one").unwrap();
    std::fs::write(
        with_suffix(&source.join(EXPORTED_DATABASE), "-wal"),
        b"nor this",
    )
    .unwrap();
    let out = scratch.path().join("out");

    export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join("ferrofin.db"),
        &out,
    )
    .await
    .unwrap();

    let copied = files(&out);
    assert!(!copied.contains_key(Path::new("ferrofin.db")));
    assert!(!copied.contains_key(&with_suffix(Path::new(EXPORTED_DATABASE), "-wal")));
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);
}

#[tokio::test]
async fn a_source_that_changes_during_the_export_fails_it() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    // An output inside the source writes into the source: the digest taken
    // after the export finds the output's files there.
    let out = source.join("export");

    let error = export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join(EXPORTED_DATABASE),
        &out,
    )
    .await
    .unwrap_err();

    let DbError::ExportSourceChanged { changed } = error else {
        panic!("expected the source to be reported changed, got {error}");
    };
    assert!(changed.contains(&"export".to_owned()), "{changed:?}");
    assert!(
        changed.iter().all(|path| path.starts_with("export")),
        "{changed:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_linked_data_directory_is_copied_and_its_target_left_alone() {
    let scratch = tempfile::tempdir().unwrap();
    let shared = scratch.path().join("shared");
    database_with_unfolded_log(scratch.path(), &shared.join("jellyfin.db")).await;
    let source = scratch.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::os::unix::fs::symlink("../shared", source.join("data")).unwrap();
    let before = files(&shared);
    // Beside the source: a copied link `../shared` would lead from here to the
    // same directory, and the snapshot would be written through it.
    let out = scratch.path().join("out");

    export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join(EXPORTED_DATABASE),
        &out,
    )
    .await
    .unwrap();

    let data = std::fs::symlink_metadata(out.join("data")).unwrap();
    assert!(data.is_dir(), "a real directory, not a link");
    assert_eq!(
        files(&shared),
        before,
        "the link's target is left as it was"
    );
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);
}

#[cfg(unix)]
#[tokio::test]
async fn a_linked_database_is_read_where_sqlite_keeps_its_log() {
    let scratch = tempfile::tempdir().unwrap();
    let real = scratch.path().join("real");
    // SQLite keeps the log beside the real file, not beside the link.
    database_with_unfolded_log(scratch.path(), &real.join("ferrofin.db")).await;
    let source = scratch.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::os::unix::fs::symlink(real.join("ferrofin.db"), source.join("ferrofin.db")).unwrap();
    let before = files(&real);
    let out = scratch.path().join("out");

    export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join("ferrofin.db"),
        &out,
    )
    .await
    .unwrap();

    assert_eq!(files(&real), before);
    assert!(!out.join("ferrofin.db").exists());
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);
}

#[tokio::test]
async fn a_missing_or_foreign_database_is_refused_before_anything_is_written() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("source");
    std::fs::create_dir_all(source.join("config")).unwrap();
    std::fs::write(source.join("config/system.json"), b"{}").unwrap();
    std::fs::write(source.join("ferrofin.db"), b"not a database at all").unwrap();
    let out = scratch.path().join("out");

    for database in [source.join("missing.db"), source.join("ferrofin.db")] {
        let error = export_data_dir(&SourceDirs::new(&source), &DOCKER, &database, &out)
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::Export { .. }), "{error}");
        assert!(!out.exists(), "nothing written for {}", database.display());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn the_snapshot_keeps_the_database_files_permissions() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let database = source.join(EXPORTED_DATABASE);
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = scratch.path().join("out");

    export_data_dir(&SourceDirs::new(&source), &DOCKER, &database, &out)
        .await
        .unwrap();

    let snapshot = std::fs::metadata(out.join(EXPORTED_DATABASE)).unwrap();
    assert_eq!(snapshot.permissions().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[tokio::test]
async fn a_read_only_database_is_exported_with_its_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let database = source.join(EXPORTED_DATABASE);
    for file in [database.clone(), with_suffix(&database, "-wal")] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o400)).unwrap();
    }
    let out = scratch.path().join("out");

    export_data_dir(&SourceDirs::new(&source), &DOCKER, &database, &out)
        .await
        .unwrap();

    let snapshot = std::fs::metadata(out.join(EXPORTED_DATABASE)).unwrap();
    assert_eq!(snapshot.permissions().mode() & 0o777, 0o400);
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);
}

#[cfg(unix)]
#[tokio::test]
async fn a_database_linked_to_a_file_inside_the_source_is_not_copied_raw() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("source");
    database_with_unfolded_log(scratch.path(), &source.join("store/real.db")).await;
    std::os::unix::fs::symlink("store/real.db", source.join("ferrofin.db")).unwrap();
    let out = scratch.path().join("out");

    export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join("ferrofin.db"),
        &out,
    )
    .await
    .unwrap();

    let copied = files(&out);
    assert_eq!(
        copied.keys().collect::<Vec<_>>(),
        [Path::new(EXPORTED_DATABASE)],
        "the snapshot alone, not the database or its log"
    );
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);
}

#[cfg(unix)]
#[tokio::test]
async fn the_database_directory_keeps_its_mode_and_time_after_the_snapshot() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let data = source.join("data");
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_hours(438_288);
    std::fs::File::open(&data)
        .unwrap()
        .set_modified(old)
        .unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = scratch.path().join("out");

    let exported = export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join(EXPORTED_DATABASE),
        &out,
    )
    .await;

    let copy = std::fs::metadata(out.join("data"));
    // Writable again, so the temporary directory can be removed.
    for dir in [&data, &out.join("data")] {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    }
    exported.unwrap();
    let copy = copy.unwrap();
    assert_eq!(copy.permissions().mode() & 0o777, 0o555);
    assert_eq!(copy.modified().unwrap(), old);
}

/// A server that ran at `/data`, its metadata directory outside the data
/// directory and its cache inside it, exports in the layout of Jellyfin's
/// docker image (gl-1557): its metadata directory becomes the output's
/// `metadata`, the cache and the data directory's own stale `metadata` are
/// left out, every stored path under a directory of the source is rewritten
/// onto `/config`, a library's and the root's locations in `Data` included
/// (gl-1924), and every one of those resolves in the output.
#[tokio::test]
async fn the_export_is_in_the_docker_layout_and_every_stored_path_resolves_in_it() {
    let scratch = tempfile::tempdir().unwrap();
    let meta = scratch.path().join("meta");
    let image = format!("{}/library/ab/poster.jpg", meta.display());
    let statements = [
        "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)".to_owned(),
        r#"CREATE TABLE "BaseItems" ("Type" TEXT NOT NULL, "Path" TEXT, "Data" TEXT)"#.to_owned(),
        r#"CREATE TABLE "BaseItemImageInfos" ("Path" TEXT NOT NULL)"#.to_owned(),
        r#"CREATE TABLE "ImageInfos" ("Path" TEXT)"#.to_owned(),
        r#"INSERT INTO "BaseItems" VALUES
           ('MediaBrowser.Controller.Entities.AggregateFolder', '/data/root',
            '{"PhysicalLocationsList":["/data/root","/media/movies"]}'),
           ('MediaBrowser.Controller.Entities.UserRootFolder', '/data/root/default', NULL),
           ('MediaBrowser.Controller.Entities.CollectionFolder', '/data/root/default/Movies',
            '{"PhysicalLocationsList":["/data/root/default/Movies","/media/movies"]}'),
           ('MediaBrowser.Controller.Entities.Person', '%MetadataPath%/People/A/Actor', NULL),
           ('MediaBrowser.Controller.Entities.Movies.Movie', '/media/movies/a.mkv', NULL)"#
            .to_owned(),
        format!(r#"INSERT INTO "BaseItemImageInfos" VALUES ('{image}')"#),
        r#"INSERT INTO "ImageInfos" VALUES ('/data/config/users/a/profile.png')"#.to_owned(),
    ];
    let source = source_with_database(
        scratch.path(),
        &statements.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    for (root, file) in [
        (&source, "root/default/Movies/movies.mblink"),
        (&source, "config/users/a/profile.png"),
        (&source, "cache/images/resized.jpg"),
        (&source, "metadata/stale.jpg"),
        (&meta, "library/ab/poster.jpg"),
        (&meta, "People/A/Actor/folder.jpg"),
    ] {
        let file = root.join(file);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, b"bytes").unwrap();
    }
    let dirs = SourceDirs {
        metadata: meta.clone(),
        not_needed: vec![source.join("cache"), source.join("log")],
        ..SourceDirs::new(&source)
    };
    let (out, before) = (scratch.path().join("out"), files(&source));

    let summary = export_data_dir(&dirs, &DOCKER, &source.join(EXPORTED_DATABASE), &out)
        .await
        .unwrap();

    assert_eq!(files(&source), before, "the source is left as it was");
    let copied = files(&out);
    for kept in [
        "root/default/Movies/movies.mblink",
        "config/users/a/profile.png",
        "metadata/library/ab/poster.jpg",
        "metadata/People/A/Actor/folder.jpg",
    ] {
        assert!(copied.contains_key(Path::new(kept)), "{kept}");
    }
    assert!(
        !copied
            .keys()
            .any(|path| path.starts_with("cache") || path.ends_with("stale.jpg"))
    );
    assert_eq!(
        summary.left_out,
        [
            (PathBuf::from("metadata"), LeftOut::NotUsed),
            (PathBuf::from("cache"), LeftOut::NotNeeded)
        ]
    );
    assert_eq!(summary.edits.paths.stored, ["/data"]);
    assert_eq!(summary.edits.paths.ran_at.as_deref(), Some("/data"));
    assert_eq!(summary.edits.paths.rewritten, 7);
    let stored = exported_paths(&out).await;
    for path in [
        "/config/metadata/library/ab/poster.jpg",
        "/config/root/default/Movies",
    ] {
        assert!(stored.contains(&path.to_owned()), "{path}");
    }
    for path in &stored {
        let source_dirs = [source.as_path(), meta.as_path(), Path::new("/data")];
        assert!(
            !source_dirs
                .iter()
                .any(|dir| Path::new(path).starts_with(dir)),
            "{path}"
        );
        let Some(inside) = path.strip_prefix("/config/") else {
            assert!(
                path.starts_with("%MetadataPath%/") || path.starts_with("/media/"),
                "{path}"
            );
            continue;
        };
        assert!(out.join(inside).exists(), "{path} resolves in the output");
    }
}

/// Every path the exported database under `out` stores in `BaseItems`,
/// `BaseItemImageInfos` and `ImageInfos`, and every string value of an
/// item's `Data`.
async fn exported_paths(out: &Path) -> Vec<String> {
    let mut conn = SqliteConnectOptions::new()
        .filename(out.join(EXPORTED_DATABASE))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let stored = sqlx::query_scalar(
        r#"SELECT "Path" FROM "BaseItems" WHERE "Path" IS NOT NULL
           UNION ALL SELECT "Path" FROM "BaseItemImageInfos"
           UNION ALL SELECT "Path" FROM "ImageInfos"
           UNION ALL SELECT j.value FROM "BaseItems", json_tree("Data") AS j
                     WHERE "Data" IS NOT NULL AND j.type = 'text'"#,
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    stored
}

/// A configuration directory kept outside the data directory is copied to
/// the output's `config`, without a `migrations.xml` Jellyfin would read again,
/// and the data directory's own `config` is left out (gl-1557).
#[tokio::test]
async fn a_configuration_directory_kept_elsewhere_is_copied_without_its_migration_list() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let etc = scratch.path().join("etc");
    std::fs::create_dir_all(etc.join("users/a")).unwrap();
    std::fs::write(etc.join("system.json"), b"{\"ServerName\": \"elsewhere\"}").unwrap();
    std::fs::write(etc.join("users/a/profile.png"), b"png").unwrap();
    std::fs::write(etc.join("migrations.xml"), b"<MigrationOptions />").unwrap();
    let dirs = SourceDirs {
        config: etc.clone(),
        ..SourceDirs::new(&source)
    };
    let out = scratch.path().join("out");

    let summary = export_data_dir(&dirs, &DOCKER, &source.join(EXPORTED_DATABASE), &out)
        .await
        .unwrap();

    let copied = files(&out);
    assert_eq!(
        copied[Path::new("config/system.json")],
        b"{\"ServerName\": \"elsewhere\"}"
    );
    assert_eq!(copied[Path::new("config/users/a/profile.png")], b"png");
    assert!(!copied.contains_key(Path::new("config/migrations.xml")));
    assert_eq!(
        summary.left_out,
        [(PathBuf::from("config"), LeftOut::NotUsed)]
    );
    assert!(etc.join("migrations.xml").exists(), "the source keeps it");
}

/// A database with `statements` run on it, at `data/jellyfin.db` of a new
/// source under `scratch`.
async fn source_with_database(scratch: &Path, statements: &[&str]) -> PathBuf {
    let source = scratch.join("source");
    let database = source.join(EXPORTED_DATABASE);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut conn = SqliteConnectOptions::new()
        .filename(&database)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    for statement in statements {
        sqlx::query(sqlx::AssertSqlSafe(*statement))
            .execute(&mut conn)
            .await
            .unwrap();
    }
    sqlx::Connection::close(conn).await.unwrap();
    source
}

/// The refusal reason of an export of `source`, which must leave the source as
/// it found it.
async fn refusal(source: &Path, out: &Path) -> String {
    let before = files(source);
    let result = export_data_dir(
        &SourceDirs::new(source),
        &DOCKER,
        &source.join(EXPORTED_DATABASE),
        out,
    )
    .await;
    assert_eq!(files(source), before, "the source is left as it was");
    match result {
        Err(ferrofin_db::DbError::ExportRefused { reason }) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A database the export cannot vouch for is refused, with the cause named,
/// from the staged copy: not Ferrofin's, inconsistent, or adopted from a
/// Jellyfin release Ferrofin does not know.
#[tokio::test]
async fn a_database_the_export_cannot_vouch_for_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source_with_database(scratch.path(), &["CREATE TABLE Items (Name TEXT)"]).await;
    let reason = refusal(&source, &scratch.path().join("out")).await;
    assert!(reason.contains("not a Ferrofin database"), "{reason}");

    let scratch = tempfile::tempdir().unwrap();
    let source = source_with_database(
        scratch.path(),
        &[
            "PRAGMA foreign_keys = OFF",
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)",
            "CREATE TABLE Users (Id TEXT PRIMARY KEY)",
            "CREATE TABLE Devices (UserId TEXT REFERENCES Users(Id))",
            "INSERT INTO Devices (UserId) VALUES ('gone')",
        ],
    )
    .await;
    let reason = refusal(&source, &scratch.path().join("out")).await;
    assert!(reason.contains("foreign-key check"), "{reason}");
    assert!(reason.contains("Devices"), "{reason}");

    let scratch = tempfile::tempdir().unwrap();
    let source = source_with_database(
        scratch.path(),
        &[
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)",
            r#"CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT PRIMARY KEY, "ProductVersion" TEXT)"#,
            r#"INSERT INTO "__EFMigrationsHistory" VALUES ('20991231000000_FromTheFuture', '99.0')"#,
        ],
    )
    .await;
    let reason = refusal(&source, &scratch.path().join("out")).await;
    assert!(reason.contains("matches no Jellyfin release"), "{reason}");
}

/// A database damaged anywhere (its header, a middle page, a truncated end)
/// is refused as damaged, or as failing its integrity check, never with a raw
/// database error.
#[tokio::test]
async fn a_damaged_database_is_refused() {
    for damage in ["header", "middle", "truncated"] {
        let scratch = tempfile::tempdir().unwrap();
        let source = source_with_database(
            scratch.path(),
            &[
                "PRAGMA page_size = 4096",
                "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)",
                "CREATE TABLE Items (Name TEXT)",
                "CREATE INDEX IX_Items_Name ON Items (Name)",
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000) \
                 INSERT INTO Items SELECT printf('item %d', i) FROM n",
            ],
        )
        .await;
        let database = source.join(EXPORTED_DATABASE);
        let mut bytes = std::fs::read(&database).unwrap();
        match damage {
            // Past the magic, which the export checks before anything else.
            "header" => bytes[16..100].fill(0x5a),
            "middle" => {
                let middle = bytes.len() / 2;
                bytes[middle..middle + 4096].fill(0x5a);
            }
            _ => bytes.truncate(bytes.len() / 2),
        }
        std::fs::write(&database, bytes).unwrap();
        let reason = refusal(&source, &scratch.path().join("out")).await;
        assert!(
            reason.contains("damaged") || reason.contains("integrity check"),
            "{damage}: {reason}"
        );
    }
}

/// A write-ahead log damaged before its end, with commits after the damage,
/// is refused: SQLite would fold it in only up to the damage and drop them.
#[tokio::test]
async fn a_log_damaged_before_its_end_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let log = with_suffix(&source.join(EXPORTED_DATABASE), "-wal");
    let mut bytes = std::fs::read(&log).unwrap();
    let page_size = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let frames = (bytes.len() - 32) / (24 + page_size);
    assert!(frames > 2, "the fixture's log has {frames} frames");
    // A byte of the first frame's page: every later frame is of the same log.
    bytes[32 + 24 + 100] ^= 0xff;
    std::fs::write(&log, bytes).unwrap();
    let reason = refusal(&source, &scratch.path().join("out")).await;
    assert!(
        reason.contains("write-ahead log is damaged at frame 1"),
        "{reason}"
    );
}

/// A Ferrofin database at `database` whose items are a playlist and a box set
/// in the data directory (one by a token path, one by an absolute path under
/// `source`, as Ferrofin stores those it makes), and box sets, a playlist and a
/// movie in `media`.
async fn database_with_containers(database: &Path, source: &Path, media: &Path) {
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut conn = SqliteConnectOptions::new()
        .filename(database)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    for statement in [
        "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)".to_owned(),
        r#"CREATE TABLE "BaseItems" ("Type" TEXT NOT NULL, "Path" TEXT)"#.to_owned(),
        format!(
            r#"INSERT INTO "BaseItems" VALUES
               ('MediaBrowser.Controller.Playlists.Playlist', '%AppDataPath%/playlists/Mix'),
               ('MediaBrowser.Controller.Entities.Movies.BoxSet', '{source}/data/collections/Saga [boxset]'),
               ('MediaBrowser.Controller.Entities.Movies.BoxSet', '{media}/Heist [boxset]'),
               ('MediaBrowser.Controller.Entities.Movies.BoxSet', '{media}/Bare [boxset]'),
               ('MediaBrowser.Controller.Playlists.Playlist', '{media}/Gone [playlist]'),
               ('MediaBrowser.Controller.Entities.Movies.Movie', '{media}/Heist [boxset]/Heist.mkv')"#,
            media = media.display(),
            source = source.display()
        ),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&mut conn)
            .await
            .unwrap();
    }
    sqlx::Connection::close(conn).await.unwrap();
}

/// The `playlist.xml` of a playlist in Jellyfin's playlists folder and the
/// `collection.xml` of a `[boxset]` folder in its collections folder are left
/// out (gl-1903): each may hold what Jellyfin last wrote, which its metadata
/// refresh would merge back over the changes made since. Their folders stay,
/// and so do a `collection.xml` whose folder only that file makes a box set,
/// and files of those names elsewhere. The playlists and box sets in media
/// libraries keep theirs, and are listed.
#[tokio::test]
async fn playlist_and_collection_files_in_the_data_directory_are_left_out() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("source");
    let media = scratch.path().join("media");
    database_with_containers(&source.join(EXPORTED_DATABASE), &source, &media).await;
    let xml = b"<Item><Shares><Share><UserId>a</UserId></Share></Shares></Item>";
    for file in [
        "data/playlists/Mix/playlist.xml",
        "data/collections/Saga [boxset]/collection.xml",
        "data/collections/Saga [boxset]/poster.jpg",
        "data/collections/Odd/collection.xml",
        "data/playlists/Mix/extra/playlist.xml",
        "metadata/library/ab/playlist.xml",
    ] {
        let path = source.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, xml).unwrap();
    }
    for folder in ["Heist [boxset]", "Bare [boxset]"] {
        std::fs::create_dir_all(media.join(folder)).unwrap();
    }
    std::fs::write(media.join("Heist [boxset]/collection.xml"), xml).unwrap();
    let out = scratch.path().join("out");
    let before = files(&source);

    let summary = export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join(EXPORTED_DATABASE),
        &out,
    )
    .await
    .unwrap();

    assert_eq!(files(&source), before, "the source is left as it was");
    let copied = files(&out);
    for left_out in [
        "data/playlists/Mix/playlist.xml",
        "data/collections/Saga [boxset]/collection.xml",
    ] {
        assert!(!copied.contains_key(Path::new(left_out)), "{left_out}");
    }
    for kept in [
        "data/collections/Saga [boxset]/poster.jpg",
        "data/collections/Odd/collection.xml",
        "data/playlists/Mix/extra/playlist.xml",
        "metadata/library/ab/playlist.xml",
    ] {
        assert!(copied.contains_key(Path::new(kept)), "{kept}");
    }
    assert!(out.join("data/playlists/Mix").is_dir(), "the folder stays");
    assert_eq!(
        summary.container_files,
        [
            ("data/collections/Odd/collection.xml", ContainerFile::Kept),
            (
                "data/collections/Saga [boxset]/collection.xml",
                ContainerFile::LeftOut
            ),
            ("data/playlists/Mix/playlist.xml", ContainerFile::LeftOut),
        ]
        .map(|(path, what)| (PathBuf::from(path), what))
    );
    assert_eq!(
        summary.library_containers,
        [
            LibraryContainer {
                kind: "playlist",
                file: media.join("Gone [playlist]/playlist.xml"),
                found: false,
            },
            LibraryContainer {
                kind: "collection",
                file: media.join("Heist [boxset]/collection.xml"),
                found: true,
            },
        ]
    );
    assert!(
        media.join("Heist [boxset]/collection.xml").is_file(),
        "media is never written"
    );
}

/// A migrated Ferrofin database holding Live TV (a tuner, a guide, a timer
/// that recorded, a series timer, a channel and a programme item with a
/// user's favourite) and an LDAP-Auth link: the export reads what Jellyfin
/// keeps in files before it drops Ferrofin's tables, keeps the items Jellyfin
/// reads with the favourite on them (gl-1556), and leaves the source byte for
/// byte as it was.
#[tokio::test]
async fn live_tv_and_identity_rows_are_carried_and_the_items_kept() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("source");
    let database = source.join(EXPORTED_DATABASE);
    let user = "5386A6F7-707F-47CC-8E9E-6756292CDAFF";
    live_tv_database(&database, user).await;
    let before = files(&source);
    let out = scratch.path().join("out");

    let summary = export_data_dir(&SourceDirs::new(&source), &DOCKER, &database, &out)
        .await
        .unwrap();

    assert_eq!(
        files(&source),
        before,
        "the source is left byte for byte as it was"
    );
    assert_live_tv_carried(&summary.edits.carried, user);
    let mut conn = SqliteConnectOptions::new()
        .filename(out.join(EXPORTED_DATABASE))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let kept: Vec<(String, i64)> = sqlx::query_as(
        r#"SELECT b."Type", COALESCE(MAX(u."IsFavorite"), 0) FROM "BaseItems" b
           LEFT JOIN "UserData" u ON u."ItemId" = b."Id"
           WHERE b."Type" LIKE 'MediaBrowser.Controller.LiveTv.%' GROUP BY b."Type" ORDER BY 1"#,
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        kept,
        [
            ("MediaBrowser.Controller.LiveTv.LiveTvChannel".to_owned(), 1),
            ("MediaBrowser.Controller.LiveTv.LiveTvProgram".to_owned(), 0),
        ],
        "the channel and the programme are kept, the favourite with them"
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name GLOB 'Ferrofin*'",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

/// A migrated Ferrofin database at `database` holding Live TV and an LDAP-Auth
/// link for `user`: [`live_tv_and_identity_rows_are_carried_and_the_items_kept`]'s.
async fn live_tv_database(database: &Path, user: &str) {
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let db = Database::connect(&format!("sqlite://{}", database.display()))
        .await
        .unwrap();
    let channel = "714EE2D0-C390-F438-28ED-AFC33B39CC9A";
    for statement in [
        r#"INSERT INTO "Users" ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess", "HidePlayedInLatest",
            "InternalId", "InvalidLoginAttemptCount", "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack", "RememberAudioSelections",
            "RememberSubtitleSelections", "RowVersion", "SubtitleMode", "SyncPlayAccess",
            "Username", "NormalizedUsername")
           VALUES (?1, 'auth', 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 'reset', 1, 1, 1, 0, 0, 0,
                   'ann', 'ANN')"#,
        r#"INSERT INTO "BaseItems" ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked",
            "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem", "Name")
           VALUES (?2, 'MediaBrowser.Controller.LiveTv.LiveTvChannel', 0, 0, 0, 0, 0, 0, 0, 'Test News'),
                  ('9086A3A8-7441-6453-6DCB-4C74BF66B4C3', 'MediaBrowser.Controller.LiveTv.LiveTvProgram',
                   0, 0, 0, 0, 0, 1, 0, 'Quiz Night')"#,
        r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
            "PlaybackPositionTicks", "Played") VALUES (?2, ?1, 'key', 1, 0, 0, 0)"#,
        r#"INSERT INTO "FerrofinLiveTvTunerHosts" ("Id", "Url", "Type", "Data")
           VALUES ('t', 'http://tuner.lan/a.m3u', 'm3u', '{"Id":"t","Url":"http://tuner.lan/a.m3u"}')"#,
        r#"INSERT INTO "FerrofinLiveTvListingProviders" ("Id", "Type", "Path", "Data")
           VALUES ('g', 'xmltv', '/srv/guide.xml', '{"Id":"g","Path":"/srv/guide.xml"}')"#,
        r#"INSERT INTO "FerrofinLiveTvTimers" ("Id", "ChannelId", "StartDate", "Status", "Data", "IsManual")
           VALUES ('timer', ?2, '2026-11-22 20:00:00', 'Completed', '{"ExternalId":"x"}', 1)"#,
        r#"INSERT INTO "FerrofinLiveTvRecordings" ("Id", "ChannelId", "TimerId", "StartDate",
            "Status", "Path", "EpisodeTitle", "SeasonNumber", "IsKids")
           VALUES ('rec', ?2, 'timer', '2026-11-22 20:00:00', 'Completed', '/srv/rec.ts', 'Round 1', 1, 1)"#,
        r#"INSERT INTO "FerrofinLiveTvSeriesTimers" ("Id", "Data", "ExternalId", "SeriesId")
           VALUES ('series', '{"Name":"Quiz Night"}', 'ext', 'quiz')"#,
        r#"INSERT INTO "FerrofinExternalIdentities" ("UserId", "ProviderId", "Subject", "AvatarHash",
            "DateCreated")
           VALUES (?1, 'Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin', 'ann', 'h1',
                   '2026-11-21 00:00:00')"#,
    ] {
        // Literal test ids only, no binds: a statement takes the ones it names.
        let statement = statement
            .replace("?1", &format!("'{user}'"))
            .replace("?2", &format!("'{channel}'"));
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(db.pool())
            .await
            .unwrap();
    }
    db.close().await;
}

/// Fails unless `carried` is what [`live_tv_database`] stored for `user`.
fn assert_live_tv_carried(carried: &CarriedRows, user: &str) {
    assert_eq!(
        *carried,
        CarriedRows {
            tuner_hosts: vec![r#"{"Id":"t","Url":"http://tuner.lan/a.m3u"}"#.to_owned()],
            listing_providers: vec![r#"{"Id":"g","Path":"/srv/guide.xml"}"#.to_owned()],
            timers: vec![TimerRow {
                id: "timer".to_owned(),
                data: r#"{"ExternalId":"x"}"#.to_owned(),
                is_manual: true,
                recording: Some(RecordingRow {
                    status: "Completed".to_owned(),
                    path: Some("/srv/rec.ts".to_owned()),
                    episode_title: Some("Round 1".to_owned()),
                    season_number: Some(1),
                    is_kids: true,
                    ..RecordingRow::default()
                }),
            }],
            series_timers: vec![SeriesTimerRow {
                data: r#"{"Name":"Quiz Night"}"#.to_owned(),
                external_id: "ext".to_owned(),
                series_id: Some("quiz".to_owned()),
            }],
            identity_links: vec![IdentityLink {
                provider_id: "Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin"
                    .to_owned(),
                user_id: user.to_owned(),
                subject: "ann".to_owned(),
                avatar_hash: Some("h1".to_owned()),
            }],
            mirrored: MirroredLiveTv {
                channels: 1,
                programs: 1,
                favorite_channels: 1,
            },
        }
    );
}
