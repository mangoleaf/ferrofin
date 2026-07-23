//! Unit 7 client-contract gate: the system / configuration / admin / misc
//! surface is registered but **not yet ported**, so every one of its routes must
//! resolve to `501 Not Implemented`, never `404`.
//!
//! These 104 operations span the `System`, `Startup`, `Configuration`,
//! `Dashboard`, `ScheduledTasks`, `Plugins`, `Package`, `Branding`,
//! `Localization`, `Environment`, `ActivityLog`, `ClientLog`, `Backup`,
//! `TimeSync`, `Channels`, `User`, `UserViews`, `Tmdb`, and the intro-skipper
//! plugin (`Troubleshooting`, `Visualization`, `SkipIntro`, `SkipButtonCss`,
//! `FileTransformation`, `OpenSubtitles`) controllers/tags. A real client
//! (Wolphin) probing any of them must learn "route exists, not implemented", not
//! "no such route".
//!
//! Five of the 104 already have real First-Light handlers (`GET /System/Info`,
//! `GET /System/Info/Public`, `POST /Users/AuthenticateByName`, `GET /Users/Me`,
//! `GET /UserViews`); those return `401`/`200`, not `501`, so they are excluded
//! from the stub probe below — leaving 99 pure `501` stubs. The auth-context
//! middleware is non-rejecting and the shared `not_implemented` stub takes no
//! auth extractor, so a tokenless probe reaches the stub and yields `501`
//! deterministically.
//!
//! The full superset gate lives in `contract_superset.rs`; this test enumerates
//! the whole Unit-7 surface explicitly so a regression in any single op fails
//! with a clear signal.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use hermit_api::create_router;
use hermit_api::test_support::fake_state;
use tower::ServiceExt;

/// Every not-yet-ported `(method, path)` of the Unit-7 surface, with concrete
/// segment values where the vendored route has a `{param}`. 99 stub ops (the 104
/// tagged ops minus the 5 that now have real First-Light handlers).
const SYSTEM_ADMIN_PROBES: &[(Method, &str)] = &[
    // System
    (Method::GET, "/System/Endpoint"),
    (Method::GET, "/System/Info/Storage"),
    (Method::GET, "/System/Logs"),
    (Method::GET, "/System/Logs/Log"),
    (Method::GET, "/System/Ping"),
    (Method::POST, "/System/Ping"),
    (Method::POST, "/System/Restart"),
    (Method::POST, "/System/Shutdown"),
    // Startup
    (Method::POST, "/Startup/Complete"),
    (Method::GET, "/Startup/Configuration"),
    (Method::POST, "/Startup/Configuration"),
    (Method::GET, "/Startup/FirstUser"),
    (Method::POST, "/Startup/RemoteAccess"),
    (Method::GET, "/Startup/User"),
    (Method::POST, "/Startup/User"),
    // Configuration
    (Method::GET, "/System/Configuration"),
    (Method::POST, "/System/Configuration"),
    (Method::POST, "/System/Configuration/Branding"),
    (Method::GET, "/System/Configuration/MetadataOptions/Default"),
    (Method::GET, "/System/Configuration/network"),
    (Method::POST, "/System/Configuration/network"),
    // Dashboard
    (Method::GET, "/web/ConfigurationPage"),
    (Method::GET, "/web/ConfigurationPages"),
    // ScheduledTasks
    (Method::GET, "/ScheduledTasks"),
    (Method::DELETE, "/ScheduledTasks/Running/task-1"),
    (Method::POST, "/ScheduledTasks/Running/task-1"),
    (Method::GET, "/ScheduledTasks/task-1"),
    (Method::POST, "/ScheduledTasks/task-1/Triggers"),
    // Plugins
    (Method::GET, "/Plugins"),
    (Method::DELETE, "/Plugins/plugin-1"),
    (Method::GET, "/Plugins/plugin-1/Configuration"),
    (Method::POST, "/Plugins/plugin-1/Configuration"),
    (Method::POST, "/Plugins/plugin-1/Manifest"),
    (Method::DELETE, "/Plugins/plugin-1/1.0.0"),
    (Method::POST, "/Plugins/plugin-1/1.0.0/Disable"),
    (Method::POST, "/Plugins/plugin-1/1.0.0/Enable"),
    (Method::GET, "/Plugins/plugin-1/1.0.0/Image"),
    // Package
    (Method::GET, "/Packages"),
    (Method::POST, "/Packages/Installed/pkg-1"),
    (Method::DELETE, "/Packages/Installing/install-1"),
    (Method::GET, "/Packages/pkg-1"),
    (Method::GET, "/Repositories"),
    (Method::POST, "/Repositories"),
    // Branding
    (Method::GET, "/Branding/Configuration"),
    (Method::GET, "/Branding/Css"),
    (Method::GET, "/Branding/Css.css"),
    // Localization
    (Method::GET, "/Localization/Countries"),
    (Method::GET, "/Localization/Cultures"),
    (Method::GET, "/Localization/Options"),
    (Method::GET, "/Localization/ParentalRatings"),
    // Environment
    (Method::GET, "/Environment/DefaultDirectoryBrowser"),
    (Method::GET, "/Environment/DirectoryContents"),
    (Method::GET, "/Environment/Drives"),
    (Method::GET, "/Environment/NetworkShares"),
    (Method::GET, "/Environment/ParentPath"),
    (Method::POST, "/Environment/ValidatePath"),
    // ActivityLog
    (Method::GET, "/System/ActivityLog/Entries"),
    // ClientLog
    (Method::POST, "/ClientLog/Document"),
    // Backup
    (Method::GET, "/Backup"),
    (Method::POST, "/Backup/Create"),
    (Method::GET, "/Backup/Manifest"),
    (Method::POST, "/Backup/Restore"),
    // TimeSync
    (Method::GET, "/GetUtcTime"),
    // Tmdb
    (Method::GET, "/Tmdb/ClientConfiguration"),
    // Channels
    (Method::GET, "/Channels"),
    (Method::GET, "/Channels/Features"),
    (Method::GET, "/Channels/Items/Latest"),
    (Method::GET, "/Channels/channel-1/Features"),
    (Method::GET, "/Channels/channel-1/Items"),
    // User (excludes real handlers: POST /Users/AuthenticateByName, GET /Users/Me)
    (Method::GET, "/Users"),
    (Method::POST, "/Users"),
    (Method::POST, "/Users/AuthenticateWithQuickConnect"),
    (Method::POST, "/Users/Configuration"),
    (Method::POST, "/Users/ForgotPassword"),
    (Method::POST, "/Users/ForgotPassword/Pin"),
    (Method::POST, "/Users/New"),
    (Method::POST, "/Users/Password"),
    (Method::GET, "/Users/Public"),
    (Method::DELETE, "/Users/user-1"),
    (Method::GET, "/Users/user-1"),
    (Method::POST, "/Users/user-1/Policy"),
    // UserViews (excludes real handler: GET /UserViews)
    (Method::GET, "/UserViews/GroupingOptions"),
    // Intro-skipper plugin: Troubleshooting
    (Method::GET, "/IntroSkipper"),
    (Method::GET, "/IntroSkipper/SupportBundle"),
    // Intro-skipper plugin: Visualization
    (Method::GET, "/Intros/AnalyzerActions/season-1"),
    (Method::POST, "/Intros/AnalyzerActions/UpdateSeason"),
    (Method::POST, "/Intros/ScanSeason/series-1/season-1"),
    (Method::GET, "/Intros/ScanStatus"),
    (Method::DELETE, "/Intros/Show/series-1/season-1"),
    (Method::GET, "/Intros/Show/series-1/season-1"),
    // Intro-skipper plugin: SkipIntro
    (Method::GET, "/Episode/ep-1/IntroSkipperSegments"),
    (Method::GET, "/Episode/ep-1/Timestamps"),
    (Method::POST, "/Episode/ep-1/Timestamps"),
    (Method::POST, "/Intros/EraseTimestamps"),
    (Method::POST, "/Intros/RebuildDatabase"),
    // Intro-skipper plugin: SkipButtonCss
    (Method::POST, "/SkipButtonCss/InjectCss"),
    (Method::POST, "/SkipButtonCss/UpdateSkipDuration"),
    // Intro-skipper plugin: FileTransformation
    (Method::POST, "/FileTransformation/RegisterTransformation"),
    // OpenSubtitles plugin
    (
        Method::POST,
        "/Jellyfin.Plugin.OpenSubtitles/ValidateLoginInfo",
    ),
];

#[tokio::test]
async fn unit7_system_admin_stub_routes_return_501_not_404() {
    let router = create_router(fake_state());

    for (method, uri) in SYSTEM_ADMIN_PROBES {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(*uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "Unit-7 contract route {method} {uri} must be a registered 501 stub, got {}",
            response.status()
        );
    }
}

/// Guards the op count: the Unit-7 surface contributes exactly 104 routes to the
/// contract, of which 5 have real First-Light handlers, so 99 remain pure `501`
/// stubs. This doubles as a drift alarm for the probe table.
#[test]
fn unit7_covers_all_99_stub_ops() {
    assert_eq!(
        SYSTEM_ADMIN_PROBES.len(),
        99,
        "Unit-7 has 104 tagged ops minus 5 real handlers = 99 stubs; probe table drifted"
    );
}
