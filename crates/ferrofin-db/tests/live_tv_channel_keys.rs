//! Migration 0046 drops the key that made a Live TV channel row go with its
//! tuner host (0029's `ON DELETE CASCADE`). Jellyfin's
//! `TunerHostManager.DeleteTunerHost` only edits the configuration, and the
//! queued guide refresh removes the channels; Ferrofin's refresh prunes them
//! the same way. These tests hold the rebuild to the rows it carries over and
//! to the keys it keeps.

use std::path::Path;

use ferrofin_db::Database;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Row as _};

/// 0029, which gave the channel table its cascading key to the tuner hosts.
const MIGRATION_0029: &str = include_str!("../migrations/0029_livetv_fk_targets.sql");

/// The database URL of the file at `path`.
fn url(path: &Path) -> String {
    format!("sqlite://{}", path.display())
}

/// Builds a file database whose channel table has the shape 0046 finds (0029's
/// cascading key) and whose history lacks 0046, holding one tuner host, one of
/// its channels and one airing on that channel. The next boot applies 0046.
async fn seed_before_0046(path: &Path) {
    let db = Database::connect(&url(path)).await.expect("create");
    db.close().await;
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(false)
        .connect()
        .await
        .expect("open");
    sqlx::raw_sql(MIGRATION_0029)
        .execute(&mut conn)
        .await
        .expect("restore the 0029 shape");
    sqlx::raw_sql(r#"DELETE FROM "_sqlx_migrations" WHERE version = 46"#)
        .execute(&mut conn)
        .await
        .expect("forget 0046");
    let keys: Vec<String> = sqlx::query_scalar(
        r#"SELECT "on_delete" FROM pragma_foreign_key_list('FerrofinLiveTvChannels')"#,
    )
    .fetch_all(&mut conn)
    .await
    .expect("keys");
    assert_eq!(keys, ["CASCADE"], "seeded with the key 0046 drops");
    sqlx::raw_sql(
        r#"INSERT INTO "FerrofinLiveTvTunerHosts" ("Id","Url","Type","Data")
               VALUES ('f6e7a1b2c3d4e5f60718293a4b5c6d7e','http://tuner/list.m3u','m3u','{}');
           INSERT INTO "FerrofinLiveTvChannels"
               ("Id","TunerHostId","TvgId","Name","Number","ImageUrl","ChannelType",
                "StreamUrl","SortIndex","DateCreated","ExternalId","IsHd","VideoCodec","AudioCodec")
               VALUES ('CHANNEL-1','f6e7a1b2c3d4e5f60718293a4b5c6d7e','news.example','News','4.1',
                       'http://x/logo.png','Tv','http://tuner/1.ts',3,'2026-10-24T00:00:00Z',
                       'm3u_abc',1,'h264','aac');
           INSERT INTO "FerrofinLiveTvPrograms" ("Id","ChannelId","StartDate","Title")
               VALUES ('PROGRAM-1','CHANNEL-1','2026-10-24T06:00:00Z','Morning Show');"#,
    )
    .execute(&mut conn)
    .await
    .expect("seed");
}

#[tokio::test]
async fn the_rebuild_keeps_every_channel_column_and_the_airings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ferrofin.db");
    seed_before_0046(&path).await;

    let db = Database::connect(&url(&path))
        .await
        .expect("boot through 0046");
    db.run_migrations().await.expect("migrate");

    let row = sqlx::query(
        r#"SELECT "Id","TunerHostId","TvgId","Name","Number","ImageUrl","ChannelType",
                  "StreamUrl","SortIndex","DateCreated","ExternalId","IsHd","VideoCodec","AudioCodec"
           FROM "FerrofinLiveTvChannels""#,
    )
    .fetch_one(db.pool())
    .await
    .expect("the channel survives");
    assert_eq!(row.get::<String, _>("Id"), "CHANNEL-1");
    assert_eq!(
        row.get::<String, _>("TunerHostId"),
        "f6e7a1b2c3d4e5f60718293a4b5c6d7e"
    );
    assert_eq!(row.get::<String, _>("TvgId"), "news.example");
    assert_eq!(row.get::<String, _>("Name"), "News");
    assert_eq!(
        row.get::<Option<String>, _>("Number").as_deref(),
        Some("4.1")
    );
    assert_eq!(
        row.get::<Option<String>, _>("ImageUrl").as_deref(),
        Some("http://x/logo.png")
    );
    assert_eq!(row.get::<String, _>("ChannelType"), "Tv");
    assert_eq!(row.get::<String, _>("StreamUrl"), "http://tuner/1.ts");
    assert_eq!(row.get::<i64, _>("SortIndex"), 3);
    assert_eq!(
        row.get::<Option<String>, _>("DateCreated").as_deref(),
        Some("2026-10-24T00:00:00Z")
    );
    assert_eq!(row.get::<String, _>("ExternalId"), "m3u_abc");
    assert_eq!(row.get::<Option<i64>, _>("IsHd"), Some(1));
    assert_eq!(
        row.get::<Option<String>, _>("VideoCodec").as_deref(),
        Some("h264")
    );
    assert_eq!(
        row.get::<Option<String>, _>("AudioCodec").as_deref(),
        Some("aac")
    );
    let programs: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinLiveTvPrograms""#)
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(programs, 1, "the rebuild cascades nothing");

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master
           WHERE type = 'index' AND tbl_name = 'FerrofinLiveTvChannels' AND sql IS NOT NULL
           ORDER BY name",
    )
    .fetch_all(db.pool())
    .await
    .expect("indexes");
    assert_eq!(
        indexes,
        [
            "FerrofinIX_LiveTvChannels_TunerHostId",
            "FerrofinIX_LiveTvChannels_TvgId"
        ]
    );
}

#[tokio::test]
async fn a_deleted_tuner_leaves_its_channels_and_a_deleted_channel_takes_its_airings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ferrofin.db");
    seed_before_0046(&path).await;
    let db = Database::connect(&url(&path))
        .await
        .expect("boot through 0046");
    db.run_migrations().await.expect("migrate");

    let channel_keys: Vec<String> = sqlx::query_scalar(
        r#"SELECT "table" FROM pragma_foreign_key_list('FerrofinLiveTvChannels')"#,
    )
    .fetch_all(db.pool())
    .await
    .expect("keys");
    assert!(channel_keys.is_empty(), "{channel_keys:?}");

    // Foreign keys are enforced on the pool: the host goes alone.
    db.delete_all_live_tv_tuner_hosts()
        .await
        .expect("delete hosts");
    let channels: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinLiveTvChannels""#)
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(
        channels, 1,
        "the guide refresh removes it, not the host's delete"
    );
    let violations: Vec<String> =
        sqlx::query_scalar(r#"SELECT "table" FROM pragma_foreign_key_check"#)
            .fetch_all(db.pool())
            .await
            .expect("check");
    assert!(violations.is_empty(), "{violations:?}");

    // The programme key still cascades.
    sqlx::query(r#"DELETE FROM "FerrofinLiveTvChannels" WHERE "Id" = 'CHANNEL-1'"#)
        .execute(db.writer())
        .await
        .expect("delete channel");
    let programs: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinLiveTvPrograms""#)
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(programs, 0);
}
