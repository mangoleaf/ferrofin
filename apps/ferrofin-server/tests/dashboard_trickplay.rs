//! Dashboard library trickplay settings through the real server and tile routes.

#[path = "support/trickplay.rs"]
mod fixture;

use fixture::Fixture;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extraction_flag_controls_generation_pruning_and_disabled_user_grid_discovery() {
    let mut fixture = Fixture::start().await;
    fixture.run_task("RefreshTrickplayImages").await;
    assert_eq!(fixture.extractions(), 0, "default extraction is disabled");
    assert_eq!(fixture.tile(2).await.status(), 404);
    fixture.options["EnableTrickplayImageExtraction"] = json!(true);
    fixture.save_options().await;
    fixture.run_task("RefreshTrickplayImages").await;
    fixture.wait_tile(2).await;
    assert_eq!(fixture.extractions(), 1);
    fixture.run_task("RefreshTrickplayImages").await;
    assert_eq!(
        fixture.extractions(),
        1,
        "existing generated data is reused"
    );
    fixture.options["EnableTrickplayImageExtraction"] = json!(false);
    fixture.save_options().await;
    fixture.run_task("RefreshTrickplayImages").await;
    assert_eq!(fixture.extractions(), 1);
    assert_eq!(
        fixture.tile(2).await.status(),
        404,
        "managed internal tiles are pruned"
    );
    fixture.options["SaveTrickplayWithMedia"] = json!(true);
    fixture.save_options().await;
    fixture.seed_user_grid();
    let before = std::fs::read(fixture.sidecar_root().join("2 - 1x1/0.jpg")).unwrap();
    fixture.run_task("RefreshTrickplayImages").await;
    assert_eq!(
        fixture.extractions(),
        1,
        "disabled sidecar discovery cannot start ffmpeg"
    );
    assert_eq!(
        std::fs::read(fixture.sidecar_root().join("2 - 1x1/0.jpg")).unwrap(),
        before
    );
    let playlist = fixture
        .client
        .get(format!(
            "{}/Videos/{}/Trickplay/2/tiles.m3u8",
            fixture.base, fixture.movie
        ))
        .header("Authorization", &fixture.auth)
        .send()
        .await
        .unwrap();
    assert!(
        playlist.status().is_success(),
        "catalogued user grid exposes a playlist"
    );
    assert!(playlist.text().await.unwrap().contains("LAYOUT=1x1"));
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_custom_provider_service_error_still_updates_the_refresh_stamp() {
    use ferrofin_traits::persistence::ItemRepository as _;
    use std::sync::Arc;
    let mut fixture = Fixture::start().await;
    fixture.options["ExtractTrickplayImagesDuringLibraryScan"] = json!(true);
    fixture.save_options().await;
    let grid = fixture.internal_root().join("999 - 1x1");
    std::fs::create_dir_all(&grid).unwrap();
    fixture.seed_user_grid();
    std::fs::copy(
        fixture.sidecar_root().join("2 - 1x1/0.jpg"),
        grid.join("0.jpg"),
    )
    .unwrap();
    let original_tile = std::fs::read(grid.join("0.jpg")).unwrap();
    let db = ferrofin_db::Database::connect(
        &ferrofin_server::config::Config::test_stub(fixture.tmp.path()).database_url(),
    )
    .await
    .unwrap();
    // A real repository failure stops discovery before disabled cleanup. An
    // undecodable JPEG returns zero dimensions in the source and is not an error.
    sqlx::query(
        r#"CREATE TRIGGER FerrofinTestRejectTrickplay BEFORE INSERT ON "TrickplayInfos"
           BEGIN SELECT RAISE(ABORT, 'fixture rejects trickplay discovery'); END"#,
    )
    .execute(db.writer())
    .await
    .unwrap();
    let repo = ferrofin_core::FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ferrofin_core::ItemTypeLookup::new()),
    );
    let id = uuid::Uuid::parse_str(&fixture.movie).unwrap();
    let before = repo
        .retrieve_item(id)
        .await
        .unwrap()
        .unwrap()
        .date_last_refreshed;
    let etag = fixture.refresh("FullRefresh", false).await;
    fixture.wait_etag(&etag).await;
    let after = repo
        .retrieve_item(id)
        .await
        .unwrap()
        .unwrap()
        .date_last_refreshed;
    assert!(
        after > before,
        "RunCustomProvider sets ErrorMessage without counting a failure, so ordinary errors still permit DateLastRefreshed"
    );
    assert_eq!(fixture.extractions(), 0);
    assert_eq!(
        std::fs::read(grid.join("0.jpg")).unwrap(),
        original_tile,
        "repository error precedes disabled cleanup"
    );
    sqlx::query("DROP TRIGGER FerrofinTestRejectTrickplay")
        .execute(db.writer())
        .await
        .unwrap();
    let etag = fixture.refresh("FullRefresh", false).await;
    fixture.wait_etag(&etag).await;
    assert!(
        !grid.exists(),
        "without the trigger, the same discovery succeeds and disabled cleanup removes the grid"
    );
    assert_eq!(fixture.extractions(), 0);
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_extraction_flag_and_live_timing_change_refresh_completion_without_restart() {
    let mut fixture = Fixture::start().await;
    fixture.options["EnableTrickplayImageExtraction"] = json!(true);
    fixture.save_options().await;
    let before = fixture.refresh("FullRefresh", true).await;
    fixture.wait_etag(&before).await;
    assert_eq!(
        fixture.extractions(),
        0,
        "the separate during-scan flag defaults off even for forced refresh"
    );
    fixture.options["ExtractTrickplayImagesDuringLibraryScan"] = json!(true);
    fixture.save_options().await;
    fixture.hold();
    let before = fixture.refresh("FullRefresh", false).await;
    fixture.wait_extractions(1).await;
    assert_eq!(
        fixture.item().await["Etag"],
        before,
        "Blocking preserves the old metadata save boundary while extraction is held"
    );
    fixture.release();
    fixture.wait_etag(&before).await;
    fixture.wait_tile(2).await;
    let before = fixture.refresh("FullRefresh", false).await;
    fixture.wait_etag(&before).await;
    assert_eq!(
        fixture.extractions(),
        1,
        "normal refresh reuses generated data"
    );
    fixture.set_behavior("NonBlocking").await;
    fixture.hold();
    let before = fixture.refresh("FullRefresh", true).await;
    fixture.wait_extractions(2).await;
    fixture.wait_etag(&before).await;
    assert_eq!(
        fixture.tile(2).await.status(),
        404,
        "NonBlocking saves while the replacement job is still held"
    );
    fixture.release();
    fixture.wait_tile(2).await;
    fixture.options["ExtractTrickplayImagesDuringLibraryScan"] = json!(false);
    fixture.save_options().await;
    let before = fixture.refresh("FullRefresh", true).await;
    fixture.wait_etag(&before).await;
    assert_eq!(
        fixture.extractions(),
        2,
        "disabling scan extraction applies to the next refresh"
    );
    fixture.options["ExtractTrickplayImagesDuringLibraryScan"] = json!(true);
    fixture.options["EnableTrickplayImageExtraction"] = json!(false);
    fixture.save_options().await;
    fixture.set_behavior("Blocking").await;
    let before = fixture.refresh("FullRefresh", false).await;
    fixture.wait_etag(&before).await;
    assert_eq!(
        fixture.tile(2).await.status(),
        404,
        "the scan provider invokes the manager's disabled prune branch"
    );
    let before = fixture.refresh("FullRefresh", true).await;
    fixture.wait_etag(&before).await;
    fixture.wait_tile(2).await;
    assert_eq!(
        fixture.extractions(),
        3,
        "FullRefresh regeneration is forced even with automatic extraction disabled"
    );
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_destination_and_migration_follow_live_options_and_preserve_dotted_media_names() {
    let mut fixture = Fixture::start_with_name("Movie.Name.mkv").await;
    fixture.options["EnableTrickplayImageExtraction"] = json!(true);
    fixture.save_options().await;
    fixture.run_task("RefreshTrickplayImages").await;
    fixture.wait_tile(2).await;
    let tile = "2 - 1x1/0.jpg";
    let original = std::fs::read(fixture.internal_root().join(tile)).unwrap();
    for sidecar in [true, false] {
        fixture.options["SaveTrickplayWithMedia"] = json!(sidecar);
        fixture.save_options().await;
        assert_eq!(
            fixture.tile(2).await.status(),
            404,
            "lookup switches immediately before migration moves files"
        );
        fixture.run_task("MoveTrickplayImages").await;
        fixture.wait_tile(2).await;
        let (selected, old) = if sidecar {
            (fixture.sidecar_root(), fixture.internal_root())
        } else {
            (fixture.internal_root(), fixture.sidecar_root())
        };
        assert_eq!(std::fs::read(selected.join(tile)).unwrap(), original);
        assert!(!old.exists());
        assert_eq!(
            fixture.extractions(),
            1,
            "migration keeps encoded tiles instead of regenerating"
        );
    }
    fixture.options["SaveTrickplayWithMedia"] = json!(true);
    fixture.options["EnableTrickplayImageExtraction"] = json!(false);
    fixture.save_options().await;
    fixture.run_task("MoveTrickplayImages").await;
    assert!(
        fixture.internal_root().join(tile).is_file(),
        "disabled extraction prevents migration"
    );
    fixture.options["EnableTrickplayImageExtraction"] = json!(true);
    fixture.save_options().await;
    fixture.run_task("MoveTrickplayImages").await;
    assert!(fixture.sidecar_root().join(tile).is_file());
    assert!(
        !fixture.media.join("Movie.trickplay").exists(),
        "the entire dotted basename is preserved"
    );
    std::fs::remove_dir_all(fixture.sidecar_root()).unwrap();
    std::fs::write(fixture.sidecar_root(), b"blocked destination").unwrap();
    fixture.run_task("RefreshTrickplayImages").await;
    assert_eq!(
        std::fs::read(fixture.sidecar_root()).unwrap(),
        b"blocked destination"
    );
    assert!(
        !fixture.internal_root().exists(),
        "a failed sidecar write cannot silently choose internal storage"
    );
    assert_eq!(fixture.tile(2).await.status(), 404);
    fixture.finish().await;
}
