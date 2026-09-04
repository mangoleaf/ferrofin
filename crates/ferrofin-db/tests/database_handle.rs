//! The [`Database`] handle's own bookkeeping surface: the `FerrofinMeta`
//! key/value table, the `HomeSection` eager-load stand-ins, the consistent
//! `VACUUM INTO` snapshot, and the explicit close the composition root uses
//! around an in-process restart.

use ferrofin_db::Database;
use ferrofin_db::entities::display_preferences::HomeSectionEntity;

/// A migrated in-memory database.
async fn memory_db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    db
}

#[tokio::test]
async fn meta_round_trips_and_replaces_on_conflict() {
    let db = memory_db().await;
    assert_eq!(db.meta_get("unset").await.expect("read unset"), None);

    db.meta_set("import_marker", "v1").await.expect("write");
    assert_eq!(
        db.meta_get("import_marker").await.expect("read").as_deref(),
        Some("v1")
    );

    db.meta_set("import_marker", "v2").await.expect("rewrite");
    assert_eq!(
        db.meta_get("import_marker").await.expect("read").as_deref(),
        Some("v2"),
        "a second write to the same key replaces the value"
    );
}

/// Inserts the user + display-preferences pair the `HomeSection` FK needs and
/// returns the display-preferences row id.
async fn display_preferences_row(db: &Database, id: i64, user: &str) -> i64 {
    sqlx::query(
        r#"INSERT INTO "Users" (
            "Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections", "RowVersion",
            "SubtitleMode", "SyncPlayAccess", "Username"
        ) VALUES (?1, 'auth', 0, 0, 0, 0, 0, 1, 0, ?2, 0, 5, 0, 'reset', 1, 1, 1, 0, 0, 0, ?3)"#,
    )
    .bind(user)
    .bind(id)
    .bind(format!("user-{id}"))
    .execute(db.writer())
    .await
    .expect("insert user");
    sqlx::query(
        r#"INSERT INTO "DisplayPreferences" ("Id", "ChromecastVersion", "Client",
            "EnableNextVideoInfoOverlay", "ItemId", "ScrollDirection", "ShowBackdrop",
            "ShowSidebar", "SkipBackwardLength", "SkipForwardLength", "UserId")
            VALUES (?1, 0, 'web', 0, '00000000-0000-0000-0000-000000000000', 0, 1, 0, 10, 30, ?2)"#,
    )
    .bind(id)
    .bind(user)
    .execute(db.writer())
    .await
    .expect("insert display preferences");
    id
}

#[tokio::test]
async fn home_sections_are_replaced_wholesale_per_display_preferences_row() {
    let db = memory_db().await;
    let mine = display_preferences_row(&db, 1, "00000000-0000-0000-0000-0000000000A1").await;
    let theirs = display_preferences_row(&db, 2, "00000000-0000-0000-0000-0000000000A2").await;
    assert!(db.home_sections(mine).await.expect("empty").is_empty());

    // Written out of order; read back by `Order`.
    db.replace_home_sections(mine, &[(2, 6), (0, 1), (1, 4)])
        .await
        .expect("write mine");
    db.replace_home_sections(theirs, &[(0, 7)])
        .await
        .expect("write theirs");

    let rows: Vec<(i32, i32)> = db
        .home_sections(mine)
        .await
        .expect("read mine")
        .iter()
        .map(|s: &HomeSectionEntity| (s.order, s.type_))
        .collect();
    assert_eq!(rows, vec![(0, 1), (1, 4), (2, 6)]);

    // A replace clears the OLD set for this row only — the other user's
    // sections are untouched.
    db.replace_home_sections(mine, &[(0, 9)])
        .await
        .expect("rewrite mine");
    let mine_after: Vec<(i32, i32)> = db
        .home_sections(mine)
        .await
        .expect("reread mine")
        .iter()
        .map(|s| (s.order, s.type_))
        .collect();
    assert_eq!(mine_after, vec![(0, 9)]);
    let theirs_after = db.home_sections(theirs).await.expect("reread theirs");
    assert_eq!(theirs_after.len(), 1);
    assert_eq!(i64::from(theirs_after[0].display_preferences_id), theirs);
    assert_eq!(theirs_after[0].type_, 7);

    // Replacing with nothing leaves the row sectionless.
    db.replace_home_sections(mine, &[]).await.expect("clear");
    assert!(db.home_sections(mine).await.expect("cleared").is_empty());
}

#[tokio::test]
async fn snapshot_is_a_complete_copy_and_refuses_an_existing_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}", dir.path().join("live.db").display());
    let db = Database::connect_sized(&url, Some(1))
        .await
        .expect("file connect");
    db.meta_set("snapshot_probe", "present")
        .await
        .expect("write before snapshot");

    // The destination name carries a quote to prove the path is escaped into
    // the statement rather than interpolated raw.
    let dest = dir.path().join("it's-a-backup.db");
    db.snapshot_to(&dest).await.expect("snapshot");
    assert!(dest.exists());

    // Something written AFTER the snapshot must not be in it: it is one
    // transaction's view, not a live mirror.
    db.meta_set("snapshot_probe", "changed-later")
        .await
        .expect("write after snapshot");

    let copy = Database::connect_sized(&format!("sqlite://{}", dest.display()), Some(1))
        .await
        .expect("open the snapshot");
    assert_eq!(
        copy.meta_get("snapshot_probe")
            .await
            .expect("read")
            .as_deref(),
        Some("present"),
        "the snapshot holds the value as of the VACUUM INTO"
    );

    // `VACUUM INTO` never overwrites: a second snapshot onto the same file
    // is an error, not a silent clobber of a good backup.
    let err = db
        .snapshot_to(&dest)
        .await
        .expect_err("existing destination must be refused");
    assert!(
        matches!(err, ferrofin_db::DbError::Sqlx(_)),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn close_is_idempotent_and_observable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}", dir.path().join("close.db").display());
    let db = Database::connect_sized(&url, Some(2))
        .await
        .expect("file connect");
    let clone = db.clone();
    assert!(!db.is_closed());

    db.close().await;
    assert!(db.is_closed());
    assert!(
        clone.is_closed(),
        "clones share the pools, so closing one closes the handle for every clone"
    );
    // A query on a closed handle fails cleanly rather than hanging.
    assert!(clone.meta_get("anything").await.is_err());

    // Closing again is a no-op.
    db.close().await;
    assert!(db.is_closed());

    // And the file reopens with no connection from the previous handle still
    // on it — which is the whole point of closing before a restart.
    let reopened = Database::connect_sized(&url, Some(2))
        .await
        .expect("reopen after close");
    assert!(!reopened.is_closed());
}
