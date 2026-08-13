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
        r#"INSERT INTO "HermitLinkedChildren" ("ParentId", "ChildId", "ChildType")
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
    let mut often_row = item(often, BaseItemKind::Movie, "Often");
    often_row.id = often_row.id.to_uppercase();
    let mut rarely_row = item(rarely, BaseItemKind::Movie, "Rarely");
    rarely_row.id = rarely_row.id.to_uppercase();
    persist
        .save_items(&[often_row, rarely_row])
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
