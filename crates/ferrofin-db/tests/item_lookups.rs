//! The batched per-item lookups on the [`Database`] handle — the one-query-per-
//! page reads the DTO service and the image walk lean on instead of a full row
//! per item.
//!
//! Every lookup binds its ids in `IN (…)` chunks of [`BATCH_BIND_CHUNK`], keys
//! its result by the stored (uppercase, hyphenated) GUID, and silently omits
//! ids that have nothing to report — the caller joins the result back onto its
//! page, so a missing key must mean "nothing here", never an error.

use ferrofin_db::{BATCH_BIND_CHUNK, Database, ImageParentRow, PLACEHOLDER_ITEM_ID};

/// A migrated in-memory database.
async fn db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    db
}

/// A stored GUID with the given low byte, in Jellyfin's uppercase hyphenated
/// form.
fn guid(n: u8) -> String {
    format!("00000000-0000-0000-0000-0000000000{n:02X}")
}

/// Inserts a minimal `BaseItems` row of the given stored `Type`.
async fn insert_item(db: &Database, id: &str, type_: &str) {
    sqlx::query(
        r#"INSERT INTO "BaseItems" (
            "Id", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Type"
        ) VALUES (?1, 0, 0, 0, 0, 0, 0, 0, ?2)"#,
    )
    .bind(id)
    .bind(type_)
    .execute(db.writer())
    .await
    .expect("insert base item");
}

/// Sets one nullable text column on an existing `BaseItems` row.
async fn set_text(db: &Database, id: &str, column: &str, value: Option<&str>) {
    sqlx::query(&format!(
        r#"UPDATE "BaseItems" SET "{column}" = ?1 WHERE "Id" = ?2"#
    ))
    .bind(value)
    .bind(id)
    .execute(db.writer())
    .await
    .unwrap_or_else(|e| panic!("set {column}: {e}"));
}

#[tokio::test]
async fn provider_ids_come_back_per_item_and_skip_items_without_any() {
    let db = db().await;
    let (with_two, with_one, bare) = (guid(0x71), guid(0x72), guid(0x73));
    for id in [&with_two, &with_one, &bare] {
        insert_item(&db, id, "Movie").await;
    }
    for (item, provider, value) in [
        (&with_two, "Tmdb", "603"),
        (&with_two, "Imdb", "tt0133093"),
        (&with_one, "Tvdb", "81189"),
    ] {
        sqlx::query(
            r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
               VALUES (?1, ?2, ?3)"#,
        )
        .bind(item)
        .bind(provider)
        .bind(value)
        .execute(db.writer())
        .await
        .expect("insert provider id");
    }

    let mut rows = db
        .provider_ids_for_items(&[with_two.clone(), with_one.clone(), bare, guid(0x79)])
        .await
        .expect("provider ids");
    rows.sort();
    assert_eq!(
        rows,
        vec![
            (with_two.clone(), "Imdb".to_owned(), "tt0133093".to_owned()),
            (with_two, "Tmdb".to_owned(), "603".to_owned()),
            (with_one, "Tvdb".to_owned(), "81189".to_owned()),
        ]
    );

    // An empty id list is a no-op, not a malformed `IN ()`.
    assert!(
        db.provider_ids_for_items(&[])
            .await
            .expect("empty lookup")
            .is_empty()
    );
}

/// The chunking is only observable when a page exceeds one chunk: one id
/// past the chunk size must land in a second query and still come back.
#[tokio::test]
async fn a_page_wider_than_one_bind_chunk_is_read_in_full() {
    let db = db().await;
    // One past the chunk (after the seeded placeholder id drops out).
    let ids: Vec<String> = (0..=u16::try_from(BATCH_BIND_CHUNK + 1).expect("small chunk"))
        .map(|n| format!("00000000-0000-0000-0000-00000000{n:04X}"))
        .filter(|id| id != PLACEHOLDER_ITEM_ID)
        .collect();
    assert!(
        ids.len() > BATCH_BIND_CHUNK,
        "the page must span two chunks"
    );

    let mut tx = db.writer().begin().await.expect("begin");
    for id in &ids {
        sqlx::query(
            r#"INSERT INTO "BaseItems" (
                "Id", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
                "IsRepeat", "IsSeries", "IsVirtualItem", "Type", "Data"
            ) VALUES (?1, 0, 0, 0, 0, 0, 0, 0, 'Movie', '{}')"#,
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .expect("insert item");
    }
    tx.commit().await.expect("commit");

    let blobs = db.item_data_blobs(&ids).await.expect("data blobs");
    assert_eq!(
        blobs.len(),
        ids.len(),
        "every id across both chunks answers"
    );
}

#[tokio::test]
async fn data_blobs_omit_rows_without_data() {
    let db = db().await;
    let (series, keyless) = (guid(0x10), guid(0x11));
    insert_item(&db, &series, "Series").await;
    insert_item(&db, &keyless, "Series").await;
    set_text(&db, &series, "Data", Some(r#"{"DisplayOrder":"absolute"}"#)).await;

    let blobs = db
        .item_data_blobs(&[series.clone(), keyless])
        .await
        .expect("data blobs");
    assert_eq!(
        blobs,
        vec![(series, r#"{"DisplayOrder":"absolute"}"#.to_owned())]
    );
}

#[tokio::test]
async fn studios_omit_null_and_empty_columns() {
    let db = db().await;
    let (named, empty, unset) = (guid(0x20), guid(0x21), guid(0x22));
    for id in [&named, &empty, &unset] {
        insert_item(&db, id, "Series").await;
    }
    set_text(&db, &named, "Studios", Some("HBO|BBC")).await;
    set_text(&db, &empty, "Studios", Some("")).await;

    let studios = db
        .item_studios(&[named.clone(), empty, unset])
        .await
        .expect("studios");
    assert_eq!(studios, vec![(named, "HBO|BBC".to_owned())]);
}

#[tokio::test]
async fn alternate_version_counts_group_by_primary_and_ignore_the_placeholder() {
    let db = db().await;
    let (primary, alt_a, alt_b, lonely) = (guid(0x30), guid(0x31), guid(0x32), guid(0x33));
    for id in [&primary, &alt_a, &alt_b, &lonely] {
        insert_item(&db, id, "Movie").await;
    }
    set_text(&db, &alt_a, "PrimaryVersionId", Some(&primary)).await;
    set_text(&db, &alt_b, "PrimaryVersionId", Some(&primary)).await;
    // The seeded placeholder row must never count as anyone's alternate,
    // exactly as the full-row read excludes it.
    set_text(&db, PLACEHOLDER_ITEM_ID, "PrimaryVersionId", Some(&primary)).await;

    let counts = db
        .alternate_version_counts(&[primary.clone(), lonely])
        .await
        .expect("counts");
    assert_eq!(counts, vec![(primary, 2)]);
}

#[tokio::test]
async fn image_parent_rows_project_the_walk_columns() {
    let db = db().await;
    let (root, album, bare) = (guid(0x40), guid(0x41), guid(0x42));
    insert_item(&db, &root, "MediaBrowser.Controller.Entities.Folder").await;
    insert_item(
        &db,
        &album,
        "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
    )
    .await;
    insert_item(&db, &bare, "Movie").await;
    sqlx::query(
        r#"UPDATE "BaseItems"
           SET "ParentId" = ?1, "OwnerId" = ?2, "SeriesId" = ?3, "SeasonId" = ?4,
               "AlbumArtists" = 'Nina Simone', "Path" = '/music/nina',
               "LUFS" = -14.5, "NormalizationGain" = 2.25
           WHERE "Id" = ?5"#,
    )
    .bind(&root)
    .bind(&root)
    .bind(guid(0x43))
    .bind(guid(0x44))
    .bind(&album)
    .execute(db.writer())
    .await
    .expect("decorate album");

    let mut rows = db
        .image_parent_rows(&[album.clone(), bare.clone(), guid(0x45)])
        .await
        .expect("parent rows");
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(
        rows,
        vec![
            ImageParentRow {
                id: album,
                type_: "MediaBrowser.Controller.Entities.Audio.MusicAlbum".to_owned(),
                parent_id: Some(root.clone()),
                owner_id: Some(root),
                series_id: Some(guid(0x43)),
                season_id: Some(guid(0x44)),
                album_artists: Some("Nina Simone".to_owned()),
                path: Some("/music/nina".to_owned()),
                lufs: Some(-14.5),
                normalization_gain: Some(2.25),
            },
            ImageParentRow {
                id: bare,
                type_: "Movie".to_owned(),
                parent_id: None,
                owner_id: None,
                series_id: None,
                season_id: None,
                album_artists: None,
                path: None,
                lufs: None,
                normalization_gain: None,
            },
        ]
    );
}

#[tokio::test]
async fn rows_of_type_match_the_stored_type_exactly() {
    let db = db().await;
    let collection = "MediaBrowser.Controller.Entities.CollectionFolder";
    let (lib, other) = (guid(0x50), guid(0x51));
    insert_item(&db, &lib, collection).await;
    // A suffix match would catch this; the equality the index serves must not.
    insert_item(&db, &other, "Ferrofin.Test.CollectionFolder").await;
    set_text(&db, &lib, "Path", Some("/media/movies")).await;
    set_text(
        &db,
        &lib,
        "Data",
        Some(r#"{"PhysicalLocationsList":["/media/movies"]}"#),
    )
    .await;

    let rows = db.rows_of_type(collection).await.expect("rows of type");
    assert_eq!(
        rows,
        vec![(
            lib,
            Some("/media/movies".to_owned()),
            Some(r#"{"PhysicalLocationsList":["/media/movies"]}"#.to_owned()),
        )]
    );
    assert!(
        db.rows_of_type("MediaBrowser.Controller.Entities.Nothing")
            .await
            .expect("unknown type")
            .is_empty()
    );
}

#[tokio::test]
async fn photo_album_names_skip_non_albums_and_nameless_albums() {
    let db = db().await;
    let photo_album = "MediaBrowser.Controller.Entities.PhotoAlbum";
    let (album, nameless, folder) = (guid(0x60), guid(0x61), guid(0x62));
    insert_item(&db, &album, photo_album).await;
    insert_item(&db, &nameless, photo_album).await;
    insert_item(&db, &folder, "MediaBrowser.Controller.Entities.Folder").await;
    set_text(&db, &album, "Name", Some("Holiday 2026")).await;
    set_text(&db, &folder, "Name", Some("Loose Photos")).await;

    let names = db
        .photo_album_names(&[album.clone(), nameless, folder])
        .await
        .expect("album names");
    assert_eq!(names, vec![(album, "Holiday 2026".to_owned())]);
}
