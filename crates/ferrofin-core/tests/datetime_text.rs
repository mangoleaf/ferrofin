//! Stored `DateTime` text is Jellyfin's (`yyyy-MM-dd HH:mm:ss.FFFFFFF`, gl-1618),
//! so a value Jellyfin wrote and the value Ferrofin binds for the same instant
//! are the same text: the date filters keep the bound's own item, the change
//! guards leave an unchanged row alone, the premiere-date sort ties a Jan-1
//! premiere with a year-only item, and a by-name row made on demand stores the
//! same form.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{FerrofinItemPersistenceService, FerrofinItemRepository};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use uuid::Uuid;

/// A movie row with `PremiereDate` written as the text Jellyfin stores.
async fn movie(db: &Database, id: Uuid, name: &str, premiere: Option<&str>, year: Option<i64>) {
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[BaseItemEntity {
            id: guid_to_db(id),
            type_: BaseItemKind::Movie
                .stored_type_name()
                .expect("stored")
                .to_owned(),
            name: Some(name.to_owned()),
            sort_name: Some(name.to_owned()),
            production_year: year,
            ..BaseItemEntity::default()
        }])
        .await
        .expect("save movie");
    sqlx::query(r#"UPDATE "BaseItems" SET "PremiereDate" = ?2 WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .bind(premiere)
        .execute(db.pool())
        .await
        .expect("premiere text");
}

/// A migrated in-memory database.
async fn database() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    db
}

fn repository(db: &Database) -> FerrofinItemRepository {
    FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
}

#[tokio::test]
async fn a_premiere_date_filter_keeps_the_item_at_its_bound() {
    let db = database().await;
    let film = Uuid::from_u128(0xF111);
    movie(&db, film, "Film", Some("2020-01-01 00:00:00"), Some(2020)).await;
    let bound = Utc
        .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
        .single()
        .expect("instant");
    let movies = |query: InternalItemsQuery| InternalItemsQuery {
        recursive: true,
        include_item_types: vec![BaseItemKind::Movie],
        ..query
    };
    let repo = repository(&db);
    for (label, query) in [
        (
            "MinPremiereDate",
            InternalItemsQuery {
                min_premiere_date: Some(bound),
                ..Default::default()
            },
        ),
        (
            "MaxPremiereDate",
            InternalItemsQuery {
                max_premiere_date: Some(bound),
                ..Default::default()
            },
        ),
    ] {
        let found = repo.get_item_ids(&movies(query)).await.expect("query");
        assert_eq!(found, [film], "{label}");
    }
}

#[tokio::test]
async fn a_jan_1_premiere_ties_with_a_year_only_item() {
    let db = database().await;
    let (dated, year_only) = (Uuid::from_u128(0xF111), Uuid::from_u128(0xF112));
    // Sorted by name after the tie: the year-only item first.
    movie(&db, dated, "Bravo", Some("2020-01-01 00:00:00"), Some(2020)).await;
    movie(&db, year_only, "Alpha", None, Some(2020)).await;
    let found = repository(&db)
        .get_item_ids(&InternalItemsQuery {
            recursive: true,
            include_item_types: vec![BaseItemKind::Movie],
            order_by: vec![
                (ItemSortBy::PremiereDate, SortOrder::Ascending),
                (ItemSortBy::SortName, SortOrder::Ascending),
            ],
            ..Default::default()
        })
        .await
        .expect("query");
    assert_eq!(found, [year_only, dated]);
}

#[tokio::test]
async fn an_unchanged_row_in_jellyfins_text_is_not_rewritten() {
    let db = database().await;
    let persistence = FerrofinItemPersistenceService::new(db.clone());
    let id = Uuid::from_u128(0xF111);
    let modified = Utc
        .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
        .single()
        .expect("instant");
    let row = BaseItemEntity {
        id: guid_to_db(id),
        type_: BaseItemKind::Movie
            .stored_type_name()
            .expect("stored")
            .to_owned(),
        name: Some("Film".to_owned()),
        path: Some("/media/Film.mkv".to_owned()),
        size: Some(42),
        date_modified: Some(modified),
        ..BaseItemEntity::default()
    };
    persistence
        .save_items(std::slice::from_ref(&row))
        .await
        .expect("save");
    // As Jellyfin stores them: a whole second without a fraction.
    sqlx::query(
        r#"UPDATE "BaseItems" SET "DateModified" = '2026-01-02 03:04:05',
                  "DateLastMediaAdded" = '2026-01-02 03:04:05', "DateLastSaved" = 'before'
            WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(id))
    .execute(db.pool())
    .await
    .expect("jellyfin text");

    assert!(
        !persistence
            .update_file_facts(&row, None)
            .await
            .expect("file facts")
    );
    assert!(
        !persistence
            .update_date_last_media_added(id, Some(modified))
            .await
            .expect("media added")
    );
    let saved: String =
        sqlx::query_scalar(r#"SELECT "DateLastSaved" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("saved");
    assert_eq!(saved, "before", "neither guard rewrote the row");
}

#[tokio::test]
async fn a_by_name_row_made_on_demand_stores_jellyfins_text() {
    let db = database().await;
    let id = FerrofinItemPersistenceService::new(db.clone())
        .ensure_by_name_item(BaseItemKind::Genre, "Comedy", "/metadata/Genre/Comedy")
        .await
        .expect("ensure")
        .expect("a genre row");
    let (created, modified): (String, String) =
        sqlx::query_as(r#"SELECT "DateCreated", "DateModified" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("dates");
    let parsed = chrono::NaiveDateTime::parse_from_str(&created, "%Y-%m-%d %H:%M:%S%.f")
        .expect("the space form, not RFC 3339");
    assert_eq!(datetime_to_db(parsed.and_utc()), created);
    assert_eq!(modified, created);
}
