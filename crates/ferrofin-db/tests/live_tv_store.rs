//! The Live TV configuration store on the [`Database`] handle: tuner hosts and
//! listing providers, the two `Ferrofin*` tables behind Jellyfin's
//! `LiveTvOptions.TunerHosts` / `ListingProviders`.
//!
//! The tuner count is the Live TV availability gate (C#
//! `LiveTvManager.IsLiveTvEnabled` is `TunerHosts.Length > 0`), so it must
//! track the rows exactly — a stale count either hides Live TV from every
//! client or shows an empty view.

use ferrofin_db::Database;

/// A migrated in-memory database.
async fn db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    db
}

#[tokio::test]
async fn tuner_hosts_upsert_by_id_and_drive_the_count() {
    let db = db().await;
    assert_eq!(db.live_tv_tuner_count().await.expect("count"), 0);
    assert!(db.live_tv_tuner_hosts().await.expect("hosts").is_empty());

    db.upsert_live_tv_tuner_host("B", "http://b/playlist.m3u", "m3u", "{}")
        .await
        .expect("insert b");
    db.upsert_live_tv_tuner_host("A", "http://a/old.m3u", "m3u", "{}")
        .await
        .expect("insert a");
    assert_eq!(db.live_tv_tuner_count().await.expect("count"), 2);

    // Same id: the row is replaced, not duplicated.
    db.upsert_live_tv_tuner_host("A", "http://a/new.m3u", "hdhomerun", r#"{"x":1}"#)
        .await
        .expect("update a");
    assert_eq!(db.live_tv_tuner_count().await.expect("count"), 2);
    assert_eq!(
        db.live_tv_tuner_hosts().await.expect("hosts"),
        vec![
            (
                "A".to_owned(),
                "http://a/new.m3u".to_owned(),
                "hdhomerun".to_owned(),
                r#"{"x":1}"#.to_owned(),
            ),
            (
                "B".to_owned(),
                "http://b/playlist.m3u".to_owned(),
                "m3u".to_owned(),
                "{}".to_owned(),
            ),
        ],
        "id-ordered, with the upsert's new values"
    );

    db.delete_all_live_tv_tuner_hosts().await.expect("clear");
    assert_eq!(db.live_tv_tuner_count().await.expect("count"), 0);
    assert!(db.live_tv_tuner_hosts().await.expect("hosts").is_empty());
}

#[tokio::test]
async fn listing_providers_upsert_by_id() {
    let db = db().await;
    assert!(
        db.live_tv_listing_providers()
            .await
            .expect("empty")
            .is_empty()
    );

    db.upsert_live_tv_listing_provider("P2", "xmltv", "/guide/b.xml", "{}")
        .await
        .expect("insert p2");
    db.upsert_live_tv_listing_provider("P1", "xmltv", "/guide/old.xml", "{}")
        .await
        .expect("insert p1");
    db.upsert_live_tv_listing_provider("P1", "schedulesdirect", "", r#"{"u":"x"}"#)
        .await
        .expect("update p1");

    assert_eq!(
        db.live_tv_listing_providers().await.expect("providers"),
        vec![
            ("P1".to_owned(), "schedulesdirect".to_owned(), String::new()),
            (
                "P2".to_owned(),
                "xmltv".to_owned(),
                "/guide/b.xml".to_owned()
            ),
        ]
    );
    // The listing rows never count as tuners.
    assert_eq!(db.live_tv_tuner_count().await.expect("count"), 0);
}
