//! The base URL a server lifetime serves under (gl-555). The composition root
//! resolves it after the configuration manager has imported an adopted
//! Jellyfin `network.xml`, so an install Jellyfin served under a prefix keeps
//! that prefix from its first boot.

use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::system::ServerApplicationHost as _;

#[tokio::test]
async fn an_adopted_network_xml_base_url_applies_on_the_first_boot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("network.xml"),
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <NetworkConfiguration xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n\
         <BaseUrl>/jellyfin</BaseUrl>\n\
         </NetworkConfiguration>\n",
    )
    .expect("network.xml");
    let config = Config::test_stub(tmp.path());
    let db = Database::connect_in_memory().await.expect("database");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, tx)
        .await
        .expect("wired");

    assert_eq!(wired.base_url, "/jellyfin", "the mount");
    assert_eq!(
        wired.app_host.base_url(),
        "/jellyfin",
        "the advertised URLs"
    );
}
