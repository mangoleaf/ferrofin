//! The series episode listing's narrow read (gl-1259): `get_episode_order_rows`
//! returns `get_item_list`'s rows in its order with only the columns the
//! season filter and the aired-order sort read, under the same plan.

use std::sync::Arc;

use ferrofin_core::item_type_lookup::{ItemTypeLookup, stored_type_name};
use ferrofin_core::translate_query::{QueryShape, build_query};
use ferrofin_core::{FerrofinItemPersistenceService, FerrofinItemRepository};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use sqlx::{Execute, Row};
use uuid::Uuid;

const KEY: &str = "narrow-series";

fn row(id: Uuid, kind: BaseItemKind, name: &str) -> BaseItemEntity {
    BaseItemEntity {
        id: guid_to_db(id),
        type_: stored_type_name(kind).expect("stored type").to_owned(),
        name: Some(name.to_owned()),
        sort_name: Some(name.to_lowercase()),
        overview: Some(format!("{name} overview")),
        series_presentation_unique_key: Some(KEY.to_owned()),
        ..BaseItemEntity::default()
    }
}

/// The series' episode-and-season query, as both listings run it.
fn series_query() -> InternalItemsQuery {
    InternalItemsQuery {
        series_presentation_unique_key: Some(KEY.to_owned()),
        include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Season],
        order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
        is_missing: Some(false),
        ..InternalItemsQuery::default()
    }
}

async fn plan_of(db: &Database, shape: QueryShape) -> Vec<String> {
    let mut builder = build_query(&series_query(), shape);
    let mut query = builder.build();
    let args = query.take_arguments().expect("arguments").expect("binds");
    let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
    sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
        .fetch_all(db.pool())
        .await
        .expect("explain")
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect()
}

#[tokio::test]
async fn the_narrow_read_keeps_the_rows_order_and_order_columns() {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let (series, season) = (Uuid::from_u128(0x5E), Uuid::from_u128(0x5EA));
    let mut season_row = row(season, BaseItemKind::Season, "Season 1");
    season_row.parent_id = Some(guid_to_db(series));
    season_row.index_number = Some(1);
    season_row.presentation_unique_key = Some("narrow-season-1".to_owned());
    let episode = |id: u128, name: &str, data: Option<&str>| {
        let mut episode = row(Uuid::from_u128(id), BaseItemKind::Episode, name);
        episode.parent_id = Some(guid_to_db(season));
        episode.season_id = Some(guid_to_db(season));
        episode.parent_index_number = Some(1);
        episode.index_number = Some(i64::try_from(id & 0xF).expect("index"));
        episode.path = Some(format!("/media/{name}.mkv"));
        episode.premiere_date = chrono::DateTime::from_timestamp(1_600_000_000, 0);
        episode.data = data.map(str::to_owned);
        episode
    };
    let special = r#"{"Overview":"long","AirsBeforeSeasonNumber":1,"AirsBeforeEpisodeNumber":2}"#;
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[
            BaseItemEntity {
                presentation_unique_key: Some(KEY.to_owned()),
                is_folder: true,
                ..row(series, BaseItemKind::Series, "Series")
            },
            season_row,
            episode(
                0xE1,
                "b episode",
                Some(r#"{"Overview":"long","Tagline":"x"}"#),
            ),
            episode(0xE2, "a special", Some(special)),
            episode(0xE3, "c episode", None),
        ])
        .await
        .expect("save");
    let repository = FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()));

    let full = repository
        .get_item_list(&series_query())
        .await
        .expect("full");
    let narrow = repository
        .get_episode_order_rows(&series_query())
        .await
        .expect("narrow");
    assert_eq!(full.len(), 4);
    let ids = |rows: &[BaseItemEntity]| rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&narrow), ids(&full), "the same rows in the same order");
    for (narrow, full) in narrow.iter().zip(&full) {
        let expected = BaseItemEntity {
            id: full.id.clone(),
            type_: full.type_.clone(),
            parent_id: full.parent_id.clone(),
            season_id: full.season_id.clone(),
            channel_id: full.channel_id.clone(),
            path: full.path.clone(),
            index_number: full.index_number,
            parent_index_number: full.parent_index_number,
            premiere_date: full.premiere_date,
            presentation_unique_key: full.presentation_unique_key.clone(),
            // `Data` only where it names an `Airs*` member.
            data: full.data.clone().filter(|data| data.contains("\"Airs")),
            ..BaseItemEntity::default()
        };
        assert_eq!(narrow, &expected, "{:?}", full.name);
    }
    assert_eq!(
        narrow.iter().filter(|row| row.data.is_some()).count(),
        1,
        "only the special's Data is read"
    );
    assert!(full.iter().all(|row| row.overview.is_some()));

    // The projection changes no access path.
    assert_eq!(
        plan_of(&db, QueryShape::EpisodeOrder).await,
        plan_of(&db, QueryShape::FullRows).await
    );
}
