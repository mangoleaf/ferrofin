//! `ferrofin_db::export::export_data_dir`, the copy of a stopped data directory
//! that `ferrofin-server export-jellyfin` writes (gl-1548): the tree copied,
//! the database snapshot holding what the write-ahead log held, the source
//! left as it was, and a source that changes during the export refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ferrofin_db::DbError;
use ferrofin_db::export::{EXPORTED_DATABASE, export_data_dir};
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
    let out = scratch.path().join("out");
    let before = files(&source);

    let summary = export_data_dir(&source, &source.join(EXPORTED_DATABASE), &out)
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
    // The database is the snapshot alone: none of the source's companion files.
    assert_eq!(
        copied
            .keys()
            .filter(|path| path.starts_with("data"))
            .collect::<Vec<_>>(),
        [Path::new(EXPORTED_DATABASE)]
    );
    assert_eq!(summary.files, 2);
    assert_eq!(summary.database, out.join(EXPORTED_DATABASE));
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);
}

#[tokio::test]
async fn two_exports_of_one_source_hold_the_same_database() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let database = source.join(EXPORTED_DATABASE);
    let (first, second) = (scratch.path().join("first"), scratch.path().join("second"));

    export_data_dir(&source, &database, &first).await.unwrap();
    export_data_dir(&source, &database, &second).await.unwrap();

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

    export_data_dir(&source, &source.join("ferrofin.db"), &out)
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

    let error = export_data_dir(&source, &source.join(EXPORTED_DATABASE), &out)
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

    export_data_dir(&source, &source.join(EXPORTED_DATABASE), &out)
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

    export_data_dir(&source, &source.join("ferrofin.db"), &out)
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
        let error = export_data_dir(&source, &database, &out).await.unwrap_err();
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

    export_data_dir(&source, &database, &out).await.unwrap();

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

    export_data_dir(&source, &database, &out).await.unwrap();

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

    export_data_dir(&source, &source.join("ferrofin.db"), &out)
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

    let exported = export_data_dir(&source, &source.join(EXPORTED_DATABASE), &out).await;

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
    let result = export_data_dir(source, &source.join(EXPORTED_DATABASE), out).await;
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
