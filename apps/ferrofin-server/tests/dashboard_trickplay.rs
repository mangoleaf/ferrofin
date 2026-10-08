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
