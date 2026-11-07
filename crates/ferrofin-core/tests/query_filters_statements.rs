//! The legacy filter facets read the matching items' years and ratings in one
//! statement.
//!
//! `/Items/Filters` answers the distinct years, official ratings, genres and
//! tags of the items a query matches. The years and the ratings come from the
//! same `BaseItems` rows under the same scope, and used to be two statements,
//! so every filter dialog walked those rows twice (gl-1284). They are now one
//! statement whose `(ProductionYear, OfficialRating)` pairs are split in the
//! app, with upstream's values and order: distinct positive years ascending,
//! distinct non-empty ratings in SQLite's BINARY order.
//!
//! This binary records the statements one call runs ([`sql_statements`]).

mod sql_statements;

use std::sync::Arc;

use ferrofin_core::{FerrofinItemRepository, ItemTypeLookup};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use sql_statements::{record_statements, statements_of};
use uuid::Uuid;

/// Inserts a movie with `year` and `rating`.
async fn seed_movie(db: &Database, n: u128, year: Option<i32>, rating: Option<&str>) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name", "ProductionYear", "OfficialRating")
           VALUES (?1, ?2, 0, 0, 0, 1, 0, 0, 0, ?1, ?3, ?4)"#,
    )
    .bind(guid_to_db(Uuid::from_u128(0xF1_0000 + n)))
    .bind(BaseItemKind::Movie.stored_type_name())
    .bind(year)
    .bind(rating)
    .execute(db.writer())
    .await
    .expect("insert movie");
}

#[tokio::test]
async fn years_and_ratings_come_from_one_statement() {
    record_statements();
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    // A year without a rating, a rating without a year, repeats, values that
    // are no facet (a zero or negative year, an empty rating), and ratings
    // whose BINARY order puts the capitals first.
    for (n, year, rating) in [
        (1, Some(1999), Some("R")),
        (2, Some(1977), Some("PG-13")),
        (3, Some(1999), Some("pg")),
        (4, Some(2010), None),
        (5, None, Some("G")),
        (6, Some(0), Some("")),
        (7, Some(-5), Some("R")),
        (8, None, None),
    ] {
        seed_movie(&db, n, year, rating).await;
    }
    let repository = FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()));

    let (filters, statements) =
        statements_of(repository.get_query_filters_legacy(&InternalItemsQuery::default())).await;
    let filters = filters.expect("filters");

    assert_eq!(filters.years, vec![1977, 1999, 2010]);
    assert_eq!(filters.official_ratings, ["G", "PG-13", "R", "pg"]);
    let reads = statements
        .iter()
        .filter(|s| s.contains(r#""ProductionYear""#) || s.contains(r#""OfficialRating""#))
        .count();
    assert_eq!(reads, 1, "{statements:#?}");
}
