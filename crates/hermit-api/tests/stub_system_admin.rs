//! Unit 7 client-contract gate: the system / configuration / admin / misc
//! surface is registered but **not yet ported**, so every one of its routes must
//! resolve to `501 Not Implemented`, never `404`.
//!
//! These 104 operations span the `System`, `Startup`, `Configuration`,
//! `Dashboard`, `ScheduledTasks`, `Plugins`, `Package`, `Branding`,
//! `Localization`, `Environment`, `ActivityLog`, `ClientLog`, `Backup`,
//! `TimeSync`, `User`, `UserViews`, `Tmdb`, and the intro-skipper
//! plugin (`Troubleshooting`, `Visualization`, `SkipIntro`, `SkipButtonCss`,
//! `FileTransformation`, `OpenSubtitles`) controllers/tags. A real client
//! (Wolphin) probing any still-unported route must learn "route exists, not
//! implemented", not "no such route".
//!
//! 59 of the 104 now have real handlers: 5 from First-Light (`GET /System/Info`,
//! `GET /System/Info/Public`, `POST /Users/AuthenticateByName`, `GET /Users/Me`,
//! `GET /UserViews`), 19 from Batch 6 (the whole `Startup` controller plus the
//! `User` admin CRUD/policy/config/forgot-password and quick-connect login
//! routes), 1 from Batch 12 (`POST /ClientLog/Document`), 31 from Batch 13
//! (the whole `System` admin / `Configuration` / `Branding` / `Localization` /
//! `Environment` / `Dashboard` / `ActivityLog` / `TimeSync` surface), and 3 from
//! Batch 15 (`ScheduledTasks` read/run — `GET /ScheduledTasks`,
//! `GET /ScheduledTasks/{taskId}`, `POST /ScheduledTasks/Running/{taskId}`).
//! Those return `401`/`200`/`204`, not `501`, so they are excluded from the stub
//! probe below — leaving 45 pure `501` stubs. The auth-context
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
    // System / Configuration / Branding / Localization / Environment / Dashboard
    // / ActivityLog / TimeSync were ported in Batch 13, so those routes are now
    // real (`RequireAuth`-guarded → `401` for a tokenless probe) and exercised in
    // `batch13_handlers.rs` instead of being 501 stubs here. The named-config
    // `/System/Configuration/{key}` route is also real (it returns `501` only for
    // unknown keys, behind auth), so it is excluded too.
    // Startup — ported in Batch 6 (real handlers), covered by batch6_handlers.rs.
    // ScheduledTasks — read/run ported in Batch 15 and the cancel + trigger-config
    // routes ported in Batch 16 (all real, `RequireAuth`-guarded → `401` for a
    // tokenless probe), covered by batch15_handlers.rs: `GET /ScheduledTasks`,
    // `GET /ScheduledTasks/{taskId}`, `POST /ScheduledTasks/Running/{taskId}`,
    // `DELETE /ScheduledTasks/Running/{taskId}`, `POST /ScheduledTasks/{taskId}/Triggers`.
    // Plugins / Package / Repositories — ported as the Tier-1 plugin-manager
    // surface (`handlers::plugins`), so those routes are now real (`RequireAuth`-
    // guarded → `401`/`400`/`404` for a probe, never `501`) and exercised in
    // `plugins_handlers.rs` instead of being 501 stubs here. Runtime install
    // (`POST /Packages/Installed/{name}`) is an honest `400`, not a stub. See
    // `brain/PLAN_HERMIT_PLUGINS.md`.
    // Branding / Localization / Environment / ActivityLog / TimeSync — ported in
    // Batch 13, now real; covered by batch13_handlers.rs.
    // ClientLogController was ported in Batch 12, so `/ClientLog/Document` is now
    // real (`RequireAuth`-guarded → `401` for a tokenless probe) and exercised in
    // `batch12_handlers.rs` instead of being a 501 stub here.
    // Backup (GET list is implemented — empty; Create/Manifest/Restore deferred)
    (Method::POST, "/Backup/Create"),
    (Method::GET, "/Backup/Manifest"),
    (Method::POST, "/Backup/Restore"),
    // User — the admin CRUD/policy/config/forgot-password + quick-connect login
    // routes were ported in Batch 6 (real handlers), covered by
    // batch6_handlers.rs; only `AuthenticateByName`/`Me` were real before.
    // UserViews — `GET /UserViews` (First-Light) and `GET /UserViews/GroupingOptions`
    // (Batch 16) are both real now, covered by their handler tests.
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
/// contract. 5 had real First-Light handlers, Batch 6 ported 19 more (7 Startup +
/// 12 User admin/quick-connect), and Batch 12 ported `/ClientLog/Document`,
/// leaving 79 pure `501` stubs. This doubles as a drift alarm for the probe
/// table.
#[test]
fn unit7_covers_all_remaining_stub_ops() {
    assert_eq!(
        SYSTEM_ADMIN_PROBES.len(),
        20,
        "Unit-7 has 104 tagged ops minus 5 First-Light minus 19 Batch-6 minus 1 Batch-12 minus 31 Batch-13 minus 3 Batch-15 minus 3 Batch-16 (ScheduledTasks cancel + Triggers, UserViews/GroupingOptions) minus 1 portable-extras (Tmdb/ClientConfiguration) minus 15 Plugins/Packages/Repositories (Tier-1 plugin manager) minus 5 Channels (now implemented — empty results) minus 1 Backup (GET list implemented) = 20 stubs; probe table drifted"
    );
}
