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
