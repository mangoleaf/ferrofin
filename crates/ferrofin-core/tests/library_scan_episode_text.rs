//! What the episode providers' re-scan gate reads, and what it must not read.
//!
//! `Planned.entity` is rebuilt from the filesystem on every scan, so its `Name`
//! is always the file stem and its `Overview` always `None`. The gate that
//! stops every episode re-fetching its metadata on every scan therefore has to
//! consult the **stored** row — via `ItemRepository::item_text_rows` — and the
//! scan reads that once for the set it planned.
//!
//! Two properties with teeth here, both of which a mutation testing only the
//! scan's *output* would miss:
//!
//! 1. the projection round-trips (`Id` parses back, the PascalCase columns land
//!    on the right fields), and
//! 2. the read is scoped to the ids asked for. An unscoped `WHERE "Type" = ?`
//!    returns the same data for a full scan and is ~113 ms / ~30 MB on a
//!    60k-episode library — paid in full by `scan_paths`, which the library
//!    monitor runs for a single changed file.

use std::sync::Arc;

use ferrofin_core::FerrofinItemPersistenceService;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, stored_type_name};
use ferrofin_core::{FerrofinItemRepository, item_type_lookup};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use uuid::Uuid;

/// Seeds `count` episodes plus one movie, and returns the repository and the
/// episode ids in seed order.
async fn seeded(count: usize) -> (FerrofinItemRepository, Vec<Uuid>) {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = FerrofinItemPersistenceService::new(db.clone());

    let mut ids = Vec::new();
    let mut rows = Vec::new();
    for i in 0..count {
        let id = Uuid::from_u128(0x5000 + i as u128);
        ids.push(id);
        rows.push(BaseItemEntity {
            id: guid_to_db(id),
            type_: stored_type_name(BaseItemKind::Episode)
                .expect("episode type")
                .to_owned(),
            name: Some(format!("Episode {i}")),
            sort_name: Some(format!("001 - {i:04} - Episode {i}")),
            overview: Some(format!("Synopsis {i}")),
            path: Some(format!("/tv/Show/Season 01/raw.file.name.{i}.mkv")),
            ..Default::default()
        });
    }
    // A movie, so a type-scoped read that forgot its predicate is visible.
    rows.push(BaseItemEntity {
        id: guid_to_db(Uuid::from_u128(0x9999)),
        type_: stored_type_name(BaseItemKind::Movie)
            .expect("movie type")
            .to_owned(),
        name: Some("A Movie".to_owned()),
        path: Some("/movies/A Movie (2020).mkv".to_owned()),
        ..Default::default()
    });
    persistence.save_items(&rows).await.expect("seed");

    let repo = FerrofinItemRepository::new(db, Arc::new(ItemTypeLookup::new()));
    (repo, ids)
}

// The projection's columns land on the right fields and the id round-trips.
// Blanking the query body — which every scan test tolerated — fails here.
#[tokio::test]
async fn the_projection_round_trips() {
    let (repo, ids) = seeded(3).await;

    let rows = repo
        .item_text_rows(BaseItemKind::Episode, &ids)
        .await
        .expect("text rows");
    assert_eq!(rows.len(), 3, "one row per asked-for episode: {rows:?}");

    let first = rows
        .iter()
        .find(|r| r.id == guid_to_db(ids[0]))
        .expect("the seeded episode");
    assert_eq!(first.name.as_deref(), Some("Episode 0"));
    assert_eq!(first.sort_name.as_deref(), Some("001 - 0000 - Episode 0"));
    assert_eq!(first.overview.as_deref(), Some("Synopsis 0"));
    assert_eq!(
        first.path.as_deref(),
        Some("/tv/Show/Season 01/raw.file.name.0.mkv")
    );
    // The gate keys its map on this; a format the parser rejects would drop the
    // row silently and read as "no previous scan" forever.
    assert_eq!(
        Uuid::parse_str(&first.id).expect("id parses back"),
        ids[0],
        "the stored id must round-trip through guid_to_db"
    );
}

// The read is scoped to the ids asked for, not to every row of the kind. This
// is the property that keeps `scan_paths` cheap, and it is invisible to any
// test that asks for the whole library.
#[tokio::test]
async fn the_read_is_scoped_to_the_ids_asked_for() {
    let (repo, ids) = seeded(50).await;

    let one = repo
        .item_text_rows(BaseItemKind::Episode, &ids[..1])
        .await
        .expect("one");
    assert_eq!(
        one.len(),
        1,
        "asking for one episode must not return the library"
    );
    assert_eq!(one[0].id, guid_to_db(ids[0]));

    // Above the 500-id chunk boundary the query is split; the result must not
    // be truncated or duplicated.
    let all = repo
        .item_text_rows(BaseItemKind::Episode, &ids)
        .await
        .expect("all");
    assert_eq!(all.len(), 50);

    // Ids that do not exist simply yield no row — callers must not assume a
    // row per id.
    let missing = repo
        .item_text_rows(BaseItemKind::Episode, &[Uuid::from_u128(0xDEAD)])
        .await
        .expect("missing");
    assert!(missing.is_empty());
}

// The `Type` predicate is real: a movie's id asked for as an Episode yields
// nothing, and a kind with no stored type name short-circuits.
#[tokio::test]
async fn the_kind_predicate_is_applied() {
    let (repo, ids) = seeded(2).await;
    let movie = Uuid::from_u128(0x9999);

    let as_episode = repo
        .item_text_rows(BaseItemKind::Episode, &[movie])
        .await
        .expect("movie as episode");
    assert!(
        as_episode.is_empty(),
        "the Type predicate must exclude the movie"
    );

    let as_movie = repo
        .item_text_rows(BaseItemKind::Movie, &[movie])
        .await
        .expect("movie as movie");
    assert_eq!(as_movie.len(), 1, "…which the right kind then finds");

    // A kind the schema stores no type name for returns empty, not an error.
    assert!(item_type_lookup::stored_type_name(BaseItemKind::Program).is_none());
    assert!(
        repo.item_text_rows(BaseItemKind::Program, &ids)
            .await
            .expect("unstorable kind")
            .is_empty()
    );
}
