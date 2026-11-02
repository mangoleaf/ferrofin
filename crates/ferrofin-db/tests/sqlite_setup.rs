//! SQLite's process-wide setup ([`ferrofin_db::init_sqlite`]) must win the race
//! against every early open. Its own binary on purpose: the setup is a
//! one-shot per process, so no other test may open SQLite first.

/// The pre-boot history check is the first SQLite open on a set-up server; it
/// must not initialize SQLite before `MEMSTATUS` is turned off.
#[tokio::test]
async fn the_history_check_keeps_the_allocator_mutex_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("jellyfin.db");
    std::fs::File::create(&path).expect("create db file");
    ferrofin_db::inspect_migration_history(&path)
        .await
        .expect("inspect");
    assert!(
        ferrofin_db::init_sqlite(),
        "sqlite3_config(MEMSTATUS, 0) was refused: SQLite initialized first"
    );
}
