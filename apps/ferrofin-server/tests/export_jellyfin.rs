//! `ferrofin-server export-jellyfin` on a scratch data directory (gl-1548):
//! the binary copies the stopped directory, writes a database snapshot that
//! holds what the write-ahead log held, leaves the source byte for byte as it
//! was, gives the same database on a second run, and fails when the source
//! changes during the export.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

/// A data directory in Jellyfin's layout as a killed server leaves it: the
/// database's three rows still in its write-ahead log, plus artwork and a
/// configuration file.
async fn source(scratch: &Path) -> PathBuf {
    let source = scratch.join("source");
    let database = source.join("data/jellyfin.db");
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let live = scratch.join("live.db");
    let mut conn = SqliteConnectOptions::new()
        .filename(&live)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .pragma("wal_autocheckpoint", "0")
        .connect()
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
    std::fs::copy(&live, &database).unwrap();
    std::fs::copy(with_suffix(&live, "-wal"), with_suffix(&database, "-wal")).unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    std::fs::create_dir_all(source.join("metadata/library/ab/ab12")).unwrap();
    std::fs::write(
        source.join("metadata/library/ab/ab12/poster.jpg"),
        b"jpeg bytes",
    )
    .unwrap();
    std::fs::create_dir_all(source.join("root/default/Movies")).unwrap();
    std::fs::write(
        source.join("root/default/Movies/movies.mblink"),
        b"/media/movies",
    )
    .unwrap();
    source
}

/// Runs `ferrofin-server export-jellyfin --data-dir <source> --out <out>` with
/// no `FERROFIN_*` variable from the test's environment.
fn export(source: &Path, out: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrofin-server"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FERROFIN_") {
            command.env_remove(key);
        }
    }
    command
        .arg("export-jellyfin")
        .arg("--data-dir")
        .arg(source)
        .arg("--out")
        .arg(out)
        .output()
        .expect("run ferrofin-server export-jellyfin")
}

/// The `Name` of every row in the exported database's `Items`.
async fn exported_names(out: &Path) -> Vec<String> {
    let mut conn = SqliteConnectOptions::new()
        .filename(out.join("data/jellyfin.db"))
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
async fn export_jellyfin_copies_a_stopped_data_directory_and_leaves_it_untouched() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let before = files(&source);
    let (out, again) = (scratch.path().join("out"), scratch.path().join("again"));

    let run = export(&source, &out);

    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{}\n{stdout}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(stdout.contains("The source is unchanged."), "{stdout}");
    assert_eq!(
        files(&source),
        before,
        "the source is left byte for byte as it was"
    );
    let mut copied = files(&out);
    // Every file but the database, which is the snapshot alone.
    let snapshot = copied.remove(Path::new("data/jellyfin.db")).unwrap();
    let mut expected = before.clone();
    expected.retain(|path, _| !path.starts_with("data"));
    assert_eq!(copied, expected);
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);

    // B10: the same stopped source exports to the same database.
    assert!(export(&source, &again).status.success());
    assert_eq!(
        std::fs::read(again.join("data/jellyfin.db")).unwrap(),
        snapshot
    );
    assert_eq!(files(&source), before);
}

#[tokio::test]
async fn export_jellyfin_fails_when_the_source_changes_during_the_export() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    // An output inside the source writes into it, so the source the export
    // digests afterwards is not the one it copied.
    let run = export(&source, &source.join("export"));

    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success(), "{stderr}");
    assert!(
        stderr.contains("the source data directory changed during the export"),
        "{stderr}"
    );
}
