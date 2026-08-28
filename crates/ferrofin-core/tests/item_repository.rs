//! Integration tests for the unit-1 item repository, persistence, and counts.
//!
//! Each test spins up an in-memory `ferrofin-db` database (migrations applied),
//! persists rows through [`FerrofinItemPersistenceService`], then queries them back
//! through [`FerrofinItemRepository`] / [`FerrofinItemCountService`] to exercise the
//! `InternalItemsQuery` → SQL translation end to end.

use std::sync::Arc;

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemCountService, FerrofinItemPersistenceService, FerrofinItemRepository,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{
    ItemCountService, ItemPersistenceService, ItemRepository, ItemTypeLookup as _,
};
use uuid::Uuid;

/// The stored `Type` name for a kind (as the fixtures set it).
fn type_name(kind: BaseItemKind) -> String {
    ItemTypeLookup::new()
        .base_item_kind_names()
        .get(&kind)
        .cloned()
        .unwrap_or_default()
}

/// Builds a minimal item row of the given kind with a name.
fn item(id: Uuid, kind: BaseItemKind, name: &str) -> BaseItemEntity {
    BaseItemEntity {
        id: id.to_string(),
        album: None,
        album_artists: None,
        artists: None,
        audio: None,
        channel_id: None,
        clean_name: Some(name.to_lowercase()),
        community_rating: None,
        critic_rating: None,
        custom_rating: None,
        data: None,
        date_created: Some(chrono::Utc::now()),
        date_last_media_added: None,
        date_last_refreshed: None,
        date_last_saved: None,
        date_modified: None,
        end_date: None,
        episode_title: None,
        external_id: None,
        external_series_id: None,
        external_service_id: None,
        extra_type: None,
        forced_sort_name: None,
        genres: None,
        height: None,
        index_number: None,
        inherited_parental_rating_sub_value: None,
        inherited_parental_rating_value: None,
        is_folder: false,
        is_in_mixed_folder: false,
        is_locked: false,
        is_movie: kind == BaseItemKind::Movie,
        is_repeat: false,
        is_series: kind == BaseItemKind::Series,
        is_virtual_item: false,
        lufs: None,
        media_type: None,
        name: Some(name.to_owned()),
        normalization_gain: None,
        official_rating: None,
        extra_ids: None,
        original_title: None,
        overview: None,
        owner_id: None,
        parent_id: None,
        parent_index_number: None,
        path: None,
        preferred_metadata_country_code: None,
        preferred_metadata_language: None,
        premiere_date: None,
        presentation_unique_key: None,
        primary_version_id: None,
        production_locations: None,
        production_year: None,
        run_time_ticks: None,
        season_id: None,
        season_name: None,
        series_id: None,
        series_name: None,
        series_presentation_unique_key: None,
        show_id: None,
        size: None,
        sort_name: Some(name.to_owned()),
        start_date: None,
        studios: None,
        tagline: None,
        tags: None,
        top_parent_id: None,
        total_bitrate: None,
        type_: type_name(kind),
        unrated_type: None,
        width: None,
    }
}

async fn fresh_db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    db
}

fn repo(db: &Database) -> FerrofinItemRepository {
    FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
}

/// `EXPLAIN QUERY PLAN` for `sql`, flattened to one line.
async fn plan(db: &Database, sql: &str) -> String {
    use sqlx::Row as _;

    sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .fetch_all(db.pool())
        .await
        .expect("explain")
        .iter()
        .map(|r| r.get::<String, _>("detail"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[tokio::test]
async fn save_then_retrieve_roundtrips() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);

    let id = Uuid::from_u128(0x1001);
    persist
        .save_items(&[item(id, BaseItemKind::Movie, "Blade Runner")])
        .await
        .expect("save");

    let got = repository.retrieve_item(id).await.expect("retrieve");
    let got = got.expect("row present");
    assert_eq!(got.name.as_deref(), Some("Blade Runner"));
    assert!(got.is_movie);
    assert_eq!(got.type_, type_name(BaseItemKind::Movie));
}

/// `locked_item_ids` answers the library scan's "which items has the user
/// locked?" question for the whole database in one query. It must return
/// exactly the `IsLocked = 1` rows — a scan that got back everything would
/// stop refreshing unlocked items, and one that got back nothing would
/// trample a locked item's user-owned metadata.
#[tokio::test]
async fn locked_item_ids_returns_only_locked_rows() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);

    let unlocked = Uuid::from_u128(0x2001);
    let locked_a = Uuid::from_u128(0x2002);
    let locked_b = Uuid::from_u128(0x2003);
    let mut rows = vec![
        item(unlocked, BaseItemKind::Movie, "Open"),
        item(locked_a, BaseItemKind::Movie, "Pinned"),
        item(locked_b, BaseItemKind::Series, "Also Pinned"),
    ];
    rows[1].is_locked = true;
    rows[2].is_locked = true;
    persist.save_items(&rows).await.expect("save");

    let mut got = repository.locked_item_ids().await.expect("locked ids");
    got.sort();
    assert_eq!(got, vec![locked_a, locked_b]);
    assert!(
        !got.contains(&unlocked),
        "unlocked rows must not be returned"
    );
}

/// An empty answer on a library with nothing locked — the overwhelmingly
/// common case, and the one the scan pays for on every run.
#[tokio::test]
async fn locked_item_ids_is_empty_when_nothing_is_locked() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);

    persist
        .save_items(&[item(Uuid::from_u128(0x2010), BaseItemKind::Movie, "Open")])
        .await
        .expect("save");

    assert!(
        repository
            .locked_item_ids()
            .await
            .expect("locked ids")
            .is_empty()
    );
}

#[tokio::test]
async fn retrieve_rejects_nil_and_misses_absent() {
    let db = fresh_db().await;
    let repository = repo(&db);
    assert!(repository.retrieve_item(Uuid::nil()).await.is_err());
    assert!(
        repository
            .retrieve_item(Uuid::from_u128(0xDEAD))
            .await
            .expect("query")
            .is_none()
    );
}

#[tokio::test]
async fn include_item_types_filters_by_kind() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    persist
        .save_items(&[
            item(Uuid::from_u128(0x501), BaseItemKind::Movie, "A Movie"),
            item(Uuid::from_u128(0x502), BaseItemKind::Series, "A Series"),
            item(Uuid::from_u128(0x503), BaseItemKind::Movie, "B Movie"),
        ])
        .await
        .expect("save");

    let movies = InternalItemsQuery {
        include_item_types: vec![BaseItemKind::Movie],
        ..Default::default()
    };
    let rows = repository.get_item_list(&movies).await.expect("list");
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|r| r.type_ == type_name(BaseItemKind::Movie))
    );

    // The placeholder row is always excluded.
    let all = repository
        .get_item_list(&InternalItemsQuery::default())
        .await
        .expect("list all");
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn name_and_ordering_translate() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    persist
        .save_items(&[
            item(Uuid::from_u128(10), BaseItemKind::Movie, "Zodiac"),
            item(Uuid::from_u128(11), BaseItemKind::Movie, "Amelie"),
            item(Uuid::from_u128(12), BaseItemKind::Movie, "Memento"),
        ])
        .await
        .expect("save");

    // Default order is SortName ascending.
    let ordered = repository
        .get_item_list(&InternalItemsQuery::default())
        .await
        .expect("list");
    let names: Vec<_> = ordered.iter().filter_map(|r| r.name.clone()).collect();
    assert_eq!(names, vec!["Amelie", "Memento", "Zodiac"]);

    // Exact clean-name match.
    let by_name = InternalItemsQuery {
        name: Some("Memento".to_owned()),
        ..Default::default()
    };
    let hit = repository.get_item_list(&by_name).await.expect("list");
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].name.as_deref(), Some("Memento"));
}

/// `ItemSortBy::Random` has to reach the database as a per-row draw.
///
/// Two failure modes this pins, both of which pass every other test in this
/// file: the random expression is a function Ferrofin registers itself, so a
/// pool it never reached fails the query outright; and a single evaluation
/// hoisted out of the row loop would return the same page, in the same order,
/// on every call while still looking like a working endpoint.
#[tokio::test]
async fn random_order_draws_a_fresh_page_each_call() {
    use std::collections::HashSet;

    const LIBRARY: u128 = 40;
    const PAGE: usize = 5;
    const CALLS: usize = 20;

    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    for i in 0..LIBRARY {
        persist
            .save_items(&[item(
                Uuid::from_u128(0x9000 + i),
                BaseItemKind::Movie,
                &format!("Movie {i:02}"),
            )])
            .await
            .expect("save");
    }

    let query = InternalItemsQuery {
        order_by: vec![(ItemSortBy::Random, SortOrder::Descending)],
        limit: Some(i32::try_from(PAGE).expect("page fits")),
        ..Default::default()
    };
    let mut pages: Vec<Vec<String>> = Vec::with_capacity(CALLS);
    for _ in 0..CALLS {
        let rows = repository
            .get_item_list(&query)
            .await
            .expect("random-ordered page");
        assert_eq!(rows.len(), PAGE);
        pages.push(rows.iter().map(|r| r.id.clone()).collect());
    }

    // C(40,5) orderings: two calls agreeing by luck is a ~1-in-10^7 event, so
    // repeats mean the draw is not being made per call.
    let distinct: HashSet<&Vec<String>> = pages.iter().collect();
    assert!(
        distinct.len() >= CALLS - 1,
        "random pages repeat: {} distinct of {CALLS}",
        distinct.len()
    );

    // ...and the draw must range over the whole library, not a fixed corner of
    // it. Each item is missed with probability (1 - 5/40)^20 ~= 0.07, so ~37 of
    // 40 show up; 30 leaves a wide margin.
    let seen: HashSet<&String> = pages.iter().flatten().collect();
    assert!(
        seen.len() >= 30,
        "random draw confined to {} of {LIBRARY} items",
        seen.len()
    );
}

#[tokio::test]
async fn paging_limits_and_reports_total() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    for i in 0..5u128 {
        persist
            .save_items(&[item(
                Uuid::from_u128(100 + i),
                BaseItemKind::Movie,
                &format!("Movie {i}"),
            )])
            .await
            .expect("save");
    }

    let page = InternalItemsQuery {
        limit: Some(2),
        start_index: Some(1),
        ..Default::default()
    };
    let result = repository.get_items(&page).await.expect("get_items");
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.start_index, 1);
    assert_eq!(result.total_record_count, 5);
}

#[tokio::test]
async fn counts_group_by_kind() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let counts = FerrofinItemCountService::new(db.clone());
    persist
        .save_items(&[
            item(Uuid::from_u128(0x601), BaseItemKind::Movie, "M1"),
            item(Uuid::from_u128(0x602), BaseItemKind::Movie, "M2"),
            item(Uuid::from_u128(0x603), BaseItemKind::Series, "S1"),
            item(Uuid::from_u128(0x604), BaseItemKind::Episode, "E1"),
        ])
        .await
        .expect("save");

    let all = InternalItemsQuery::default();
    assert_eq!(counts.get_count(&all).await.expect("count"), 4);

    let by_kind = counts.get_item_counts(&all).await.expect("item counts");
    assert_eq!(by_kind.movie_count, 2);
    assert_eq!(by_kind.series_count, 1);
    assert_eq!(by_kind.episode_count, 1);
    // Top-level ItemCount serializes as 0: Jellyfin's LibraryController never
    // assigns it, so get_item_counts zeroes the grand total (the per-type counts
    // above still populate). The real total is available via get_count, asserted
    // as 4 above.
    assert_eq!(by_kind.item_count, 0);
}

#[tokio::test]
async fn delete_removes_rows_but_not_placeholder() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    let id = Uuid::from_u128(0x2002);
    persist
        .save_items(&[item(id, BaseItemKind::Movie, "Doomed")])
        .await
        .expect("save");
    assert!(repository.item_exists(id).await.expect("exists"));

    // Deleting the placeholder id is a no-op; deleting a real id removes it.
    let placeholder = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    persist
        .delete_items(&[placeholder, id])
        .await
        .expect("delete");
    assert!(!repository.item_exists(id).await.expect("exists"));
    assert!(repository.item_exists(placeholder).await.expect("exists"));
}

#[tokio::test]
async fn item_ids_query_returns_only_ids() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    let a = Uuid::from_u128(0xA);
    let b = Uuid::from_u128(0xB);
    persist
        .save_items(&[
            item(a, BaseItemKind::Movie, "Alpha"),
            item(b, BaseItemKind::Movie, "Beta"),
        ])
        .await
        .expect("save");

    let ids = repository
        .get_item_ids(&InternalItemsQuery::default())
        .await
        .expect("ids");
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&a) && ids.contains(&b));
}

#[tokio::test]
async fn has_subtitles_filters_on_stream_rows() {
    use ferrofin_core::FerrofinMediaStreamRepository;
    use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
    use ferrofin_traits::persistence::MediaStreamRepository;

    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    let subbed = Uuid::from_u128(0x601);
    let bare = Uuid::from_u128(0x602);
    persist
        .save_items(&[
            item(subbed, BaseItemKind::Movie, "Subbed"),
            item(bare, BaseItemKind::Movie, "Bare"),
        ])
        .await
        .expect("save");
    let streams = FerrofinMediaStreamRepository::new(db.clone());
    streams
        .save_media_streams(
            subbed,
            &[MediaStreamInfoEntity {
                item_id: subbed.to_string(),
                stream_index: 0,
                stream_type: 2, // Subtitle
                ..MediaStreamInfoEntity::default()
            }],
        )
        .await
        .expect("save streams");

    let with_subs = repository
        .get_item_list(&InternalItemsQuery {
            has_subtitles: Some(true),
            ..Default::default()
        })
        .await
        .expect("query");
    assert_eq!(with_subs.len(), 1);
    assert_eq!(with_subs[0].name.as_deref(), Some("Subbed"));

    let without = repository
        .get_item_list(&InternalItemsQuery {
            has_subtitles: Some(false),
            ..Default::default()
        })
        .await
        .expect("query");
    assert_eq!(without.len(), 1);
    assert_eq!(without[0].name.as_deref(), Some("Bare"));

    // The ids-only helper backing the DTO builder's HasSubtitles agrees.
    let flagged = streams
        .get_item_ids_with_subtitles(&[subbed, bare])
        .await
        .expect("flags");
    assert_eq!(flagged, vec![subbed]);
}

#[tokio::test]
async fn video_type_and_3d_filters_match_data_blob() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    let bluray = Uuid::from_u128(0x701);
    let plain = Uuid::from_u128(0x702);
    let mut bluray_row = item(bluray, BaseItemKind::Movie, "Disc");
    bluray_row.data = Some(r#"{"VideoType":"BluRay","Video3DFormat":"HalfSideBySide"}"#.to_owned());
    let mut plain_row = item(plain, BaseItemKind::Movie, "File");
    plain_row.data = Some(r#"{"VideoType":"VideoFile"}"#.to_owned());
    persist
        .save_items(&[bluray_row, plain_row])
        .await
        .expect("save");

    let discs = repository
        .get_item_list(&InternalItemsQuery {
            video_types: vec![ferrofin_model::entities::VideoType::BluRay],
            ..Default::default()
        })
        .await
        .expect("query");
    assert_eq!(discs.len(), 1);
    assert_eq!(discs[0].name.as_deref(), Some("Disc"));

    let three_d = repository
        .get_item_list(&InternalItemsQuery {
            is_3d: Some(true),
            ..Default::default()
        })
        .await
        .expect("query");
    assert_eq!(three_d.len(), 1);
    assert_eq!(three_d[0].name.as_deref(), Some("Disc"));

    let flat = repository
        .get_item_list(&InternalItemsQuery {
            is_3d: Some(false),
            ..Default::default()
        })
        .await
        .expect("query");
    assert_eq!(flat.len(), 1);
    assert_eq!(flat[0].name.as_deref(), Some("File"));
}

#[tokio::test]
async fn linked_child_ancestor_filter_finds_collections_of_a_library() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    let library = Uuid::from_u128(0x11B);
    let movie = Uuid::from_u128(0x801);
    let in_lib_set = Uuid::from_u128(0x802);
    let foreign_set = Uuid::from_u128(0x803);
    // Production ids are stored UPPERCASE-hyphenated (`guid_to_db`), and the
    // ancestor predicates bind that form — seed the same casing end to end.
    let upper = |row: ferrofin_db::entities::base_items::BaseItemEntity| {
        let mut row = row;
        row.id = row.id.to_uppercase();
        row
    };
    persist
        .save_items(&[
            upper(item(library, BaseItemKind::CollectionFolder, "Movies")),
            upper(item(movie, BaseItemKind::Movie, "Heat")),
            upper(item(in_lib_set, BaseItemKind::BoxSet, "Crime Films")),
            upper(item(foreign_set, BaseItemKind::BoxSet, "Empty Elsewhere")),
        ])
        .await
        .expect("save");
    // The movie descends from the library; the in-library box set links it.
    sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
        .bind(movie.to_string().to_uppercase())
        .bind(library.to_string().to_uppercase())
        .execute(db.writer())
        .await
        .expect("ancestor");
    sqlx::query(
        r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType")
           VALUES (?1, ?2, 0)"#,
    )
    .bind(in_lib_set.to_string().to_uppercase())
    .bind(movie.to_string().to_uppercase())
    .execute(db.writer())
    .await
    .expect("link");

    // The Collections-tab query: box sets whose linked children descend from
    // the library — the re-rooted form of `parentId=<library>` (a box set
    // never lives under the library itself).
    let rows = repository
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::BoxSet],
            linked_child_ancestor_ids: vec![library],
            recursive: true,
            ..Default::default()
        })
        .await
        .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name.as_deref(), Some("Crime Films"));
}

#[tokio::test]
async fn user_data_sorts_order_by_play_state() {
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;

    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    let user_id = Uuid::from_u128(0x9);
    let often = Uuid::from_u128(0x901);
    let rarely = Uuid::from_u128(0x902);
    // A query naming no scope is confined to the user's libraries (C#
    // `AddUserToQuery`), so these need a library above them — as every scanned
    // item on a real server has.
    let library = Uuid::from_u128(0x900);
    let mut library_row = item(library, BaseItemKind::CollectionFolder, "Library");
    library_row.id = library_row.id.to_uppercase();
    let mut often_row = item(often, BaseItemKind::Movie, "Often");
    often_row.id = often_row.id.to_uppercase();
    often_row.top_parent_id = Some(library.to_string().to_uppercase());
    let mut rarely_row = item(rarely, BaseItemKind::Movie, "Rarely");
    rarely_row.id = rarely_row.id.to_uppercase();
    rarely_row.top_parent_id = Some(library.to_string().to_uppercase());
    persist
        .save_items(&[library_row, often_row, rarely_row])
        .await
        .expect("save");
    // Seed the user + play state directly (this file's fixtures are raw rows;
    // the column list mirrors test_support::seed_user).
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'bob')"#,
    )
    .bind(user_id.to_string().to_uppercase())
    .execute(db.writer())
    .await
    .expect("seed user");
    // …and the permission `create_user` grants, without which the user can see
    // no library at all.
    sqlx::query(
        r#"INSERT INTO "Permissions" ("Kind", "RowVersion", "UserId", "Value")
           VALUES (?1, 1, ?2, 1)"#,
    )
    .bind(i32::from(
        ferrofin_db::enums::PermissionKind::EnableAllFolders,
    ))
    .bind(user_id.to_string().to_uppercase())
    .execute(db.writer())
    .await
    .expect("grant library access");
    for (item_id, count, date) in [
        (often, 9_i64, "2026-08-10 00:00:00.0000000"),
        (rarely, 1, "2026-01-01 00:00:00.0000000"),
    ] {
        sqlx::query(
            r#"INSERT INTO "UserData"
               ("ItemId","UserId","CustomDataKey","IsFavorite","LastPlayedDate",
                "PlayCount","PlaybackPositionTicks","Played")
               VALUES (?1,?2,?3,0,?4,?5,0,1)"#,
        )
        .bind(item_id.to_string().to_uppercase())
        .bind(user_id.to_string().to_uppercase())
        .bind(item_id.to_string())
        .bind(date)
        .bind(count)
        .execute(db.writer())
        .await
        .expect("user data");
    }

    let user: UserEntity = sqlx::query_as(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(user_id.to_string().to_uppercase())
        .fetch_one(db.pool())
        .await
        .expect("read user");
    for sort in [ItemSortBy::PlayCount, ItemSortBy::DatePlayed] {
        let rows = repository
            .get_item_list(&InternalItemsQuery {
                user: Some(user.clone()),
                order_by: vec![(sort, SortOrder::Descending)],
                ..Default::default()
            })
            .await
            .expect("query");
        let names: Vec<_> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
        assert_eq!(names, ["Often", "Rarely"], "sort {sort:?}");
    }
}

#[tokio::test]
async fn premiere_date_sort_falls_back_to_production_year() {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;

    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    // No PremiereDate anywhere — only filename-derived years.
    let mut newer = item(Uuid::from_u128(0xB01), BaseItemKind::Movie, "Newer");
    newer.production_year = Some(2020);
    let mut older = item(Uuid::from_u128(0xB02), BaseItemKind::Movie, "Older");
    older.production_year = Some(1999);
    persist.save_items(&[newer, older]).await.expect("save");

    let rows = repository
        .get_item_list(&InternalItemsQuery {
            order_by: vec![(ItemSortBy::PremiereDate, SortOrder::Descending)],
            ..Default::default()
        })
        .await
        .expect("query");
    let names: Vec<_> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
    assert_eq!(names, ["Newer", "Older"]);
}

#[tokio::test]
async fn paged_user_data_sort_with_total_count_works() {
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    // The EXACT shape the Movies grid sends for a "Play Count" sort: paged,
    // total-count enabled, multi-key SortBy=PlayCount,SortName,ProductionYear,
    // recursive under a parent, user attached.
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);
    let user_id = Uuid::from_u128(0x9);
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'bob')"#,
    )
    .bind(user_id.to_string().to_uppercase())
    .execute(db.writer())
    .await
    .expect("seed user");

    let library = Uuid::from_u128(0xC1B);
    let movie = Uuid::from_u128(0xC01);
    let mut lib_row = item(library, BaseItemKind::CollectionFolder, "Movies");
    lib_row.id = lib_row.id.to_uppercase();
    let mut movie_row = item(movie, BaseItemKind::Movie, "Heat");
    movie_row.id = movie_row.id.to_uppercase();
    movie_row.parent_id = Some(library.to_string().to_uppercase());
    persist
        .save_items(&[lib_row, movie_row])
        .await
        .expect("save");
    sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
        .bind(movie.to_string().to_uppercase())
        .bind(library.to_string().to_uppercase())
        .execute(db.writer())
        .await
        .expect("ancestor");

    let user: UserEntity = sqlx::query_as(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(user_id.to_string().to_uppercase())
        .fetch_one(db.pool())
        .await
        .expect("user");
    for sort in [
        ItemSortBy::PlayCount,
        ItemSortBy::DatePlayed,
        ItemSortBy::SeriesDatePlayed,
    ] {
        let result = repository
            .get_items(&InternalItemsQuery {
                user: Some(user.clone()),
                parent_id: library,
                recursive: true,
                include_item_types: vec![BaseItemKind::Movie],
                order_by: vec![
                    (sort, SortOrder::Descending),
                    (ItemSortBy::SortName, SortOrder::Ascending),
                    (ItemSortBy::ProductionYear, SortOrder::Ascending),
                ],
                start_index: Some(0),
                limit: Some(100),
                enable_total_record_count: true,
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("sort {sort:?} failed: {e}"));
        assert_eq!(result.total_record_count, 1, "sort {sort:?}");
    }
}

#[tokio::test]
async fn child_counts_exclude_merged_alternate_versions() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let counts = FerrofinItemCountService::new(db.clone());
    let season = Uuid::from_u128(0xD01);
    let primary = Uuid::from_u128(0xD02);
    let alternate = Uuid::from_u128(0xD03);
    let mut season_row = item(season, BaseItemKind::Season, "Season 1");
    season_row.id = season_row.id.to_uppercase();
    let mut primary_row = item(primary, BaseItemKind::Episode, "Pilot");
    primary_row.id = primary_row.id.to_uppercase();
    primary_row.parent_id = Some(season.to_string().to_uppercase());
    let mut alternate_row = item(alternate, BaseItemKind::Episode, "Pilot");
    alternate_row.id = alternate_row.id.to_uppercase();
    alternate_row.parent_id = Some(season.to_string().to_uppercase());
    // The merge-versions link: the duplicate points at its primary.
    alternate_row.primary_version_id = Some(primary.to_string().to_uppercase());
    persist
        .save_items(&[season_row, primary_row, alternate_row])
        .await
        .expect("save");

    let out = counts
        .get_child_count_batch(&[season], None)
        .await
        .expect("counts");
    assert_eq!(
        out.get(&season).copied(),
        Some(1),
        "a merged duplicate must not inflate the episode count"
    );
}

/// An explicit `sortBy=SortName` must break `SortName` ties on `Name`, the way
/// upstream's `ApplyOrder` does:
///
/// ```csharp
/// if (firstOrdering.OrderBy is ItemSortBy.Default or ItemSortBy.SortName)
/// {
///     orderedQuery = firstOrdering.SortOrder is SortOrder.Ascending
///         ? orderedQuery.ThenBy(e => e.Name)
///         : orderedQuery.ThenByDescending(e => e.Name);
/// }
/// ```
///
/// (`ItemSortBy.Default` is filtered out of `orderBy` before that branch, so it
/// fires exactly when the caller asked for `SortName`.)
///
/// Without the tiebreaker the tied rows come back in whatever order the storage
/// engine's sort produced, i.e. insertion order — which on a real library is
/// most of the page: every Person/Studio/Genre row shares the same (null)
/// `SortName`, so the whole by-name half of a mixed browse ties.
#[tokio::test]
async fn explicit_sort_name_breaks_ties_on_name() {
    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);

    // Same SortName, inserted in an order that is neither their Name order nor
    // its reverse — so a missing tiebreaker (which leaves the rows in the
    // storage engine's own order) cannot accidentally look alphabetical.
    //
    // The *descending* assertion below is the load-bearing one: an ascending
    // `SortName` sort is served by `FerrofinIX_BaseItems_SortName_Name`, whose
    // second column is `Name`, so ascending comes back alphabetical even when
    // the SQL forgets to ask for it. Descending is pinned to a real sort
    // (`SORT_PLAN_PIN`), so only the SQL can order it.
    let mut rows = Vec::new();
    for (n, name) in [(0x50u128, "Mike"), (0x51, "Zulu"), (0x52, "Alpha")] {
        let mut row = item(Uuid::from_u128(n), BaseItemKind::Movie, name);
        row.sort_name = Some("tie".to_owned());
        rows.push(row);
    }
    persist.save_items(&rows).await.expect("save");

    let ascending = InternalItemsQuery {
        order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
        ..Default::default()
    };
    let names: Vec<_> = repository
        .get_item_list(&ascending)
        .await
        .expect("list")
        .iter()
        .filter_map(|r| r.name.clone())
        .collect();
    assert_eq!(
        names,
        vec!["Alpha", "Mike", "Zulu"],
        "ties on SortName must fall back to Name ascending"
    );

    let descending = InternalItemsQuery {
        order_by: vec![(ItemSortBy::SortName, SortOrder::Descending)],
        ..Default::default()
    };
    let names: Vec<_> = repository
        .get_item_list(&descending)
        .await
        .expect("list")
        .iter()
        .filter_map(|r| r.name.clone())
        .collect();
    assert_eq!(
        names,
        vec!["Zulu", "Mike", "Alpha"],
        "a descending SortName sort takes ThenByDescending(Name)"
    );
}

/// `FerrofinIX_BaseItems_SortName_Name` (migration 0018) must serve the
/// ascending `(SortName, Name)` browse as an ordered index walk — not a table
/// scan into a sorter — and must NOT be allowed to serve the two orderings
/// whose tie order it would change (see `SORT_PLAN_PIN` in `translate_query`).
///
/// This asserts the *plan*, because the plan is the whole point: the rows come
/// back identical either way, so a lost index would be invisible to every other
/// test in this file while the query silently went back to O(library).
#[tokio::test]
async fn sort_name_browse_uses_the_index_and_the_pinned_shapes_do_not() {
    const INDEX: &str = "FerrofinIX_BaseItems_SortName_Name";

    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    persist
        .save_items(&[item(Uuid::from_u128(0x70), BaseItemKind::Movie, "Solaris")])
        .await
        .expect("save");

    let indexed = plan(
        &db,
        r#"SELECT bi.* FROM "BaseItems" AS bi ORDER BY bi."SortName" ASC, bi."Name" ASC LIMIT 100"#,
    )
    .await;
    assert!(
        indexed.contains(INDEX) && !indexed.contains("TEMP B-TREE"),
        "the ascending (SortName, Name) browse must walk {INDEX}, got: {indexed}"
    );

    for sql in [
        // Descending: the index walked backwards reverses ties.
        r#"SELECT bi.* FROM "BaseItems" AS bi ORDER BY +bi."SortName" DESC, bi."Name" DESC LIMIT 100"#,
        // No tiebreaker (the no-`sortBy` default): SortName alone is not a
        // total order, and the index would reorder every tied row.
        r#"SELECT bi.* FROM "BaseItems" AS bi ORDER BY +bi."SortName" LIMIT 100"#,
    ] {
        let pinned = plan(&db, sql).await;
        assert!(
            !pinned.contains(INDEX) && pinned.contains("TEMP B-TREE"),
            "the pinned ordering must keep its sort, got: {pinned}\n  for: {sql}"
        );
    }
}

/// A collection created the way a user creates one stays visible to that user.
///
/// A query naming no scope is confined to the user's libraries (C#
/// `AddUserToQuery`). Before `create_collection` put the box set in a
/// "Collections" library, it had no parent and no top parent, so scoping the
/// query made every collection a user had ever made disappear from `/Items`.
#[tokio::test]
async fn a_created_collection_survives_an_unscoped_user_query() {
    use ferrofin_traits::collections::{CollectionCreationOptions, CollectionManager};

    let db = fresh_db().await;
    let persist = FerrofinItemPersistenceService::new(db.clone());
    let repository = repo(&db);

    // One scanned movie in a library, as a real server has.
    let library = Uuid::from_u128(0xAB01);
    let movie = Uuid::from_u128(0xAB02);
    let mut library_row = item(library, BaseItemKind::CollectionFolder, "Library");
    library_row.id = library_row.id.to_uppercase();
    let mut movie_row = item(movie, BaseItemKind::Movie, "A Movie");
    movie_row.id = movie_row.id.to_uppercase();
    movie_row.top_parent_id = Some(library.to_string().to_uppercase());
    persist
        .save_items(&[library_row, movie_row])
        .await
        .expect("save");

    // …and a collection made through the real manager.
    let collections = collection_manager_over(&db);
    collections
        .create_collection(&CollectionCreationOptions {
            name: "My Collection".to_owned(),
            ..Default::default()
        })
        .await
        .expect("create collection");

    let user = seed_user_who_sees_everything(&db, Uuid::from_u128(0xABCD)).await;
    let rows = repository
        .get_item_list(&InternalItemsQuery {
            user: Some(user),
            recursive: true,
            ..Default::default()
        })
        .await
        .expect("query");
    let names: Vec<&str> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
    assert!(
        names.contains(&"My Collection"),
        "a user-created collection must still be listed, got {names:?}"
    );
    assert!(
        names.contains(&"A Movie"),
        "and so must the library's items"
    );

    // …and it went into the *Collections* container, not into whichever media
    // library happened to sort first. A media library already exists above, so
    // a by-type match would have filed it under "Library".
    let boxset = rows
        .iter()
        .find(|r| r.name.as_deref() == Some("My Collection"))
        .expect("the collection row");
    let parent = boxset.parent_id.clone().expect("a collection has a parent");
    assert_ne!(
        parent,
        library.to_string().to_uppercase(),
        "a collection must not be filed into a media library"
    );
    let container: (Option<String>, Option<String>) =
        sqlx::query_as(r#"SELECT "Name", "Path" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(&parent)
            .fetch_one(db.pool())
            .await
            .expect("container row");
    assert_eq!(container.0.as_deref(), Some("Collections"));
    assert!(
        container.1.is_some_and(|p| p.ends_with("/collections")),
        "the container is the folder under the data directory"
    );
}

/// Seeds a user with the one permission that decides whether they can see any
/// library at all — a missing `Permissions` row reads as `false`, which makes a
/// bare fixture user maximally restricted rather than minimal.
async fn seed_user_who_sees_everything(
    db: &ferrofin_db::Database,
    id: Uuid,
) -> ferrofin_db::entities::users::UserEntity {
    let key = id.to_string().to_uppercase();
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'bob')"#,
    )
    .bind(key.clone())
    .execute(db.writer())
    .await
    .expect("seed user");
    sqlx::query(
        r#"INSERT INTO "Permissions" ("Kind", "RowVersion", "UserId", "Value")
           VALUES (?1, 1, ?2, 1)"#,
    )
    .bind(i32::from(
        ferrofin_db::enums::PermissionKind::EnableAllFolders,
    ))
    .bind(key.clone())
    .execute(db.writer())
    .await
    .expect("grant library access");
    sqlx::query_as(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(key.clone())
        .fetch_one(db.pool())
        .await
        .expect("read user")
}

/// The real collection manager over `db`, with throwaway application paths.
fn collection_manager_over(db: &ferrofin_db::Database) -> ferrofin_core::FerrofinCollectionManager {
    ferrofin_core::FerrofinCollectionManager::new(
        db.clone(),
        std::sync::Arc::new(ferrofin_core::FerrofinLibraryManager::new(
            std::sync::Arc::new(ferrofin_core::FerrofinItemRepository::new(
                db.clone(),
                std::sync::Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new()),
            )),
            std::sync::Arc::new(ferrofin_core::FerrofinItemCountService::new(db.clone())),
            std::sync::Arc::new(FerrofinItemPersistenceService::new(db.clone())),
            std::sync::Arc::new(ferrofin_core::FerrofinPeopleRepository::new(db.clone())),
        )),
        std::sync::Arc::new(ferrofin_core::FerrofinLinkedChildrenService::new(
            db.clone(),
        )),
        std::sync::Arc::new(ferrofin_core::FerrofinServerApplicationPaths::new(
            "/tmp/ferrofin-test",
            "/tmp/ferrofin-test/log",
            "/tmp/ferrofin-test/config",
            "/tmp/ferrofin-test/cache",
            "/tmp/ferrofin-test/web",
        )),
    )
}
