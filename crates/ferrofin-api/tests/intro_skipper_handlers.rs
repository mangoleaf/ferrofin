//! Handler tests for the Intro Skipper extension routes that do not depend on
//! the (94-method) `LibraryManager` — the segment/task/branding/plugin surface.
//! Library-backed routes (timestamps, season episodes) are exercised end-to-end
//! against a running server; here we drive the rest through a fake `AppState`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    AuthedAuthService, FakeActivity, FakeApiKeys, FakeAppHost, FakeAuthContext,
    FakeClientEventLogger, FakeCollections, FakeDevices, FakeDisplayPreferences, FakeDto,
    FakeFileSystem, FakeLibrary, FakeLocalization, FakeLyrics, FakeMediaSources, FakeMusic,
    FakePaths, FakePlaylists, FakeProviders, FakeSearch, FakeSessions, FakeSimilarItems,
    FakeSubtitles, FakeSystem, FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews, FakeUsers,
};
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::intro_skipper::{AnalysisMode as Mode, AnalyzerAction as Action};
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_model::tasks::{TaskInfo, TaskState};
use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_segments::{MediaSegmentManager, MediaSegmentProviderInfo};
use ferrofin_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};
use ferrofin_traits::system::ServerApplicationPaths;
use ferrofin_traits::tasks::TaskManager;
use tower::ServiceExt;
use uuid::Uuid;

const INTRO_SKIPPER_ID: Uuid = Uuid::from_u128(0xc83d_86bb_a1e0_4c35_a113_e210_1cf4_ee6b);

// --- Minimal working fakes for the four small managers the routes touch ------

/// In-memory media-segment store: enough of the trait for erase to work.
#[derive(Default)]
struct MemSegments {
    rows: Mutex<Vec<(String, MediaSegmentDto)>>,
}

#[async_trait]
impl MediaSegmentManager for MemSegments {
    async fn is_type_supported(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn create_segment(
        &self,
        segment: &MediaSegmentDto,
        provider: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        let mut dto = segment.clone();
        dto.id = Uuid::new_v4();
        self.rows
            .lock()
            .unwrap()
            .push((provider.to_owned(), dto.clone()));
        Ok(dto)
    }
    async fn delete_segment(&self, segment_id: Uuid) -> Result<(), ServiceError> {
        self.rows
            .lock()
            .unwrap()
            .retain(|(_, s)| s.id != segment_id);
        Ok(())
    }
    async fn delete_segments(&self, item_id: Uuid) -> Result<(), ServiceError> {
        self.rows
            .lock()
            .unwrap()
            .retain(|(_, s)| s.item_id != item_id);
        Ok(())
    }
    async fn delete_provider_segments(
        &self,
        item_id: Uuid,
        provider: &str,
        type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        self.rows.lock().unwrap().retain(|(p, s)| {
            !(s.item_id == item_id && p == provider && type_filter.is_none_or(|t| t == s.type_))
        });
        Ok(())
    }
    async fn delete_all_provider_segments(
        &self,
        provider: &str,
        type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        self.rows
            .lock()
            .unwrap()
            .retain(|(p, s)| !(p == provider && type_filter.is_none_or(|t| t == s.type_)));
        Ok(())
    }
    async fn get_segments(
        &self,
        item_id: Uuid,
        type_filter: Option<&[MediaSegmentType]>,
        _by_provider: bool,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.item_id == item_id)
            .filter(|(_, s)| type_filter.is_none_or(|ts| ts.contains(&s.type_)))
            .map(|(_, s)| s.clone())
            .collect())
    }
    async fn has_segments(&self, item_id: Uuid) -> Result<bool, ServiceError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .any(|(_, s)| s.item_id == item_id))
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

/// A task manager whose detection task reports a fixed running state.
struct MemTasks {
    running: bool,
    started: Mutex<Vec<String>>,
}

#[async_trait]
impl TaskManager for MemTasks {
    async fn get_tasks(&self) -> Result<Vec<TaskInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskInfo>, ServiceError> {
        Ok(Some(TaskInfo {
            name: Some(task_id.to_owned()),
            state: if self.running {
                TaskState::Running
            } else {
                TaskState::Idle
            },
            current_progress_percentage: None,
            id: Some(task_id.to_owned()),
            last_execution_result: None,
            triggers: Vec::new(),
            description: None,
            category: None,
            is_hidden: false,
            key: Some(task_id.to_owned()),
        }))
    }
    async fn start_task(&self, task_id: &str) -> Result<(), ServiceError> {
        self.started.lock().unwrap().push(task_id.to_owned());
        Ok(())
    }
    async fn cancel_task(&self, _task_id: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn update_triggers(
        &self,
        _task_id: &str,
        _triggers: &[ferrofin_model::tasks::TaskTriggerInfo],
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// A config manager backing only the branding get/set the CSS routes use.
#[derive(Default)]
struct MemConfig {
    branding: Mutex<BrandingOptions>,
}

#[async_trait]
impl ServerConfigurationManager for MemConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(FakePaths)
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(ServerConfiguration::default()))
    }
    async fn update_configuration(
        &self,
        _configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        Ok(self.branding.lock().unwrap().clone())
    }
    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError> {
        *self.branding.lock().unwrap() = branding.clone();
        Ok(())
    }
}

/// A plugin manager that knows the Intro Skipper extension (version + config).
struct MemPlugins;

#[async_trait]
impl PluginManager for MemPlugins {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        Ok((id == INTRO_SKIPPER_ID).then(|| PluginDescriptor {
            id,
            name: "Intro Skipper".to_owned(),
            version: "1.2.3".to_owned(),
            description: String::new(),
            enabled: true,
            has_image: false,
            can_uninstall: false,
            configuration_file_name: None,
        }))
    }
    async fn enable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn disable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn remove_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_plugin_configuration(&self, _id: Uuid) -> Result<Vec<u8>, ServiceError> {
        Ok(br#"{"SkipbuttonHideDelay":11}"#.to_vec())
    }
    async fn set_plugin_configuration(
        &self,
        _id: Uuid,
        _config: Vec<u8>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn plugin_image(&self, _id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
        Ok(None)
    }
    async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn set_repositories(
        &self,
        _repositories: Vec<RepositoryInfo>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

/// Builds an authenticated `AppState` with the four working fakes wired in.
fn build_app(segments: Arc<MemSegments>, tasks: Arc<MemTasks>, config: Arc<MemConfig>) -> AppState {
    // Every route but the two episode reads is `RequiresElevation` upstream.
    build_app_as(
        segments,
        tasks,
        config,
        Arc::new(ferrofin_api::test_support::ApiKeyAuthService),
    )
}

/// [`build_app`] with the authentication seam chosen by the caller, so the
/// elevation-gated route can be driven as a plain user *and* as an API key.
fn build_app_as(
    segments: Arc<MemSegments>,
    tasks: Arc<MemTasks>,
    config: Arc<MemConfig>,
    auth: Arc<dyn ferrofin_traits::net::AuthService>,
) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        config,
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        auth,
        Arc::new(ferrofin_api::test_support::FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(FakeSubtitles),
        Arc::new(FakeLyrics),
        segments,
        Arc::new(FakeTrickplay),
        Arc::new(FakeDevices),
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeApiKeys),
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        tasks,
    )
    .with_plugins(Arc::new(MemPlugins))
}

async fn send(app: AppState, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", "tok")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = create_router(app).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn state() -> (Arc<MemSegments>, AppState) {
    let seg = Arc::new(MemSegments::default());
    let app = build_app(
        seg.clone(),
        Arc::new(MemTasks {
            running: false,
            started: Mutex::new(Vec::new()),
        }),
        Arc::new(MemConfig::default()),
    );
    (seg, app)
}

/// One recorded cache erase: the items (all when `None`) and the mode.
type CacheErase = (Option<Vec<Uuid>>, Option<Mode>);

/// An analysis runtime that records what the routes ask of it.
#[derive(Default)]
struct FakeAnalysis {
    running: std::sync::atomic::AtomicBool,
    rescans: Mutex<Vec<Uuid>>,
    erased: Mutex<Vec<CacheErase>>,
}

#[async_trait]
impl ferrofin_traits::intro_skipper::IntroSkipperAnalysis for FakeAnalysis {
    fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::SeqCst)
    }
    async fn rescan(&self, season_id: Uuid) -> Result<bool, ServiceError> {
        if self.running.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(false);
        }
        self.rescans.lock().unwrap().push(season_id);
        Ok(true)
    }
    async fn erase_cache(
        &self,
        items: Option<&[Uuid]>,
        mode: Option<Mode>,
    ) -> Result<u64, ServiceError> {
        self.erased
            .lock()
            .unwrap()
            .push((items.map(<[Uuid]>::to_vec), mode));
        Ok(0)
    }
    async fn clear_excluded(
        &self,
    ) -> Result<ferrofin_traits::intro_skipper::ExcludedClear, ServiceError> {
        Ok(ferrofin_traits::intro_skipper::ExcludedClear {
            affected_items: 2,
            removed_segments: 3,
            removed_cache_entries: 4,
        })
    }
    // The routes never drive automatic analysis.
    async fn items_changed(&self, _added: &[Uuid], _updated: &[Uuid], _removed: &[Uuid]) {}
    async fn task_completed(&self, _key: &str, _completed: bool) {}
    async fn plugin_configuration_changed(&self, _plugin_id: Uuid) {}
}

/// `ExcludedTimestamps/Clear` answers the dashboard's PascalCase counts.
#[tokio::test]
async fn clear_excluded_reports_its_counts() {
    let analysis = Arc::new(FakeAnalysis::default());
    let (status, body) = send(
        with_analysis(&analysis),
        "POST",
        "/Intros/ExcludedTimestamps/Clear",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"{"AffectedItems":2,"RemovedSegments":3,"RemovedCacheEntries":4}"#
    );
    // Detached (no extension): a 500.
    let (status, _) = send(state().1, "POST", "/Intros/ExcludedTimestamps/Clear", "").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

fn with_analysis(analysis: &Arc<FakeAnalysis>) -> AppState {
    let (_seg, app) = state();
    app.with_intro_skipper_analysis(Arc::clone(analysis) as _)
}

/// `ScanStatus` is the plugin's `ScheduledTaskSemaphore.IsBusy`, and
/// `ScanSeason` takes it: 202 and a rescan of that season, or 409 while a pass
/// runs.
#[tokio::test]
async fn scan_season_takes_the_scan_lock_and_status_reports_it() {
    let analysis = Arc::new(FakeAnalysis::default());
    let (status, body) = send(with_analysis(&analysis), "GET", "/Intros/ScanStatus", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"isRunning":false}"#);

    let (series, season) = (Uuid::new_v4(), Uuid::new_v4());
    let uri = format!("/Intros/ScanSeason/{series}/{season}");
    let (status, _) = send(with_analysis(&analysis), "POST", &uri, "").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(analysis.rescans.lock().unwrap().as_slice(), [season]);

    let (_, body) = send(with_analysis(&analysis), "GET", "/Intros/ScanStatus", "").await;
    assert_eq!(body, r#"{"isRunning":true}"#);
    let (status, _) = send(with_analysis(&analysis), "POST", &uri, "").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// `eraseCache` erases the mode's cached fingerprints — only for
/// Introduction and Credits, as upstream.
#[tokio::test]
async fn erase_timestamps_erases_the_cache_when_asked() {
    let analysis = Arc::new(FakeAnalysis::default());
    for query in [
        "mode=Credits&eraseCache=true",
        "mode=Recap&eraseCache=true",
        "mode=Introduction",
    ] {
        let (status, _) = send(
            with_analysis(&analysis),
            "POST",
            &format!("/Intros/EraseTimestamps?{query}"),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{query}");
    }
    assert_eq!(
        analysis.erased.lock().unwrap().as_slice(),
        [(None, Some(Mode::Credits))]
    );
}

#[tokio::test]
async fn erase_timestamps_deletes_matching_provider_rows() {
    let (seg, app) = state();
    let item = Uuid::new_v4();
    // Two IntroSkipper rows (Intro + Outro) and one other-provider row.
    seg.rows.lock().unwrap().push((
        ferrofin_traits::intro_skipper::provider_id(),
        MediaSegmentDto {
            id: Uuid::new_v4(),
            item_id: item,
            type_: MediaSegmentType::Intro,
            start_ticks: 0,
            end_ticks: 1,
        },
    ));
    seg.rows.lock().unwrap().push((
        ferrofin_traits::intro_skipper::provider_id(),
        MediaSegmentDto {
            id: Uuid::new_v4(),
            item_id: item,
            type_: MediaSegmentType::Outro,
            start_ticks: 0,
            end_ticks: 1,
        },
    ));
    seg.rows.lock().unwrap().push((
        "Other".to_owned(),
        MediaSegmentDto {
            id: Uuid::new_v4(),
            item_id: item,
            type_: MediaSegmentType::Intro,
            start_ticks: 0,
            end_ticks: 1,
        },
    ));

    let other = Uuid::from_u128(0x54);
    app.intro_skipper
        .update_timestamp(stored(other, Mode::Introduction, 0.0, 30.0))
        .await
        .unwrap();
    app.intro_skipper
        .update_timestamp(stored(other, Mode::Credits, 900.0, 960.0))
        .await
        .unwrap();
    // `[FromQuery] AnalysisMode mode` is non-nullable: absent is Introduction.
    let (status, _) = send(app.clone(), "POST", "/Intros/EraseTimestamps", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let left = app.intro_skipper.segments(other).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].mode, Mode::Credits);
    let (status, _) = send(app.clone(), "POST", "/Intros/EraseTimestamps?mode=99", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(app, "POST", "/Intros/EraseTimestamps?mode=Introduction", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let rows = seg.rows.lock().unwrap();
    // The IntroSkipper Intro row is gone; the Outro and the other provider stay.
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(
        |(p, s)| !(*p == ferrofin_traits::intro_skipper::provider_id()
            && s.type_ == MediaSegmentType::Intro)
    ));
}

#[tokio::test]
async fn erase_timestamps_rejects_bad_mode() {
    let (_seg, app) = state();
    let (status, _) = send(app, "POST", "/Intros/EraseTimestamps?mode=bogus", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn plugin_metadata_and_support_bundle() {
    let (_seg, app) = state();
    let (status, body) = send(app.clone(), "GET", "/IntroSkipper", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"version":"1.2.3"}"#);

    let (status, body) = send(app.clone(), "GET", "/MediaSegmentsApi", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"version":"1.2.3"}"#);

    let (status, body) = send(app, "GET", "/IntroSkipper/SupportBundle", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Plugin version: 1.2.3"));
    assert!(body.contains("Runs on:"));
    // The fingerprinter probes still report, and report a bool — the ffmpeg one
    // runs as a child process the handler awaits rather than blocks on.
    for line in [
        "Chromaprint (ffmpeg muxer) available: ",
        "Chromaprint (fpcalc) available: ",
    ] {
        let at = body
            .find(line)
            .unwrap_or_else(|| panic!("{line:?} missing from bundle: {body:?}"));
        let value = body[at + line.len()..].lines().next().unwrap_or_default();
        assert!(
            value == "true" || value == "false",
            "{line:?} carries a bool, got {value:?}"
        );
    }
}

#[tokio::test]
async fn no_op_success_routes() {
    let (_seg, app) = state();
    for (method, uri, body) in [("POST", "/Intros/RebuildDatabase", "")] {
        let (status, _) = send(app.clone(), method, uri, body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{method} {uri}");
    }
}

fn stored(
    item: Uuid,
    mode: Mode,
    start: f64,
    end: f64,
) -> ferrofin_traits::intro_skipper::StoredSegment {
    ferrofin_traits::intro_skipper::StoredSegment {
        item_id: item,
        mode,
        start,
        end,
        is_user_provided: false,
        config_hash: String::new(),
    }
}

/// `GET Episode/{id}/IntroSkipperSegments` reads the plugin's tier
/// (`GetTimestampsAsync`: the earliest per mode) and keys the dictionary by the
/// `AnalysisMode` names, as STJ writes enum dictionary keys.
#[tokio::test]
async fn skippable_segments_come_from_the_tier_keyed_by_mode_name() {
    let (_seg, app) = state();
    let item = Uuid::from_u128(0x51);
    for segment in [
        stored(item, Mode::Introduction, 40.0, 70.0),
        stored(item, Mode::Commercial, 600.0, 630.0),
        stored(item, Mode::Commercial, 300.0, 330.0),
    ] {
        app.intro_skipper.update_timestamp(segment).await.unwrap();
    }
    let (status, body) = send(
        app,
        "GET",
        &format!("/Episode/{item}/IntroSkipperSegments"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["Introduction"]["Start"], 40.0);
    assert_eq!(json["Commercial"]["Start"], 300.0, "the earliest of a mode");
    assert_eq!(json.as_object().unwrap().len(), 2);
}

/// `GET Intros/DisabledEpisodes/{SeasonId}` lists the season's excluded
/// episodes as `"N"`-form GUIDs (an unknown season has none).
#[tokio::test]
async fn disabled_episodes_list_the_seasons_exclusions() {
    let (_seg, app) = state();
    let (season, a, b) = (
        Uuid::from_u128(0x60),
        Uuid::from_u128(0x61),
        Uuid::from_u128(0x62),
    );
    app.intro_skipper
        .set_excluded(season, b, true)
        .await
        .unwrap();
    app.intro_skipper
        .set_excluded(season, a, true)
        .await
        .unwrap();
    app.intro_skipper
        .set_excluded(Uuid::from_u128(0x63), a, true)
        .await
        .unwrap();
    let (status, body) = send(
        app.clone(),
        "GET",
        &format!("/Intros/DisabledEpisodes/{season}"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, format!(r#"["{}","{}"]"#, a.simple(), b.simple()));
    let (_, body) = send(
        app,
        "GET",
        &format!("/Intros/DisabledEpisodes/{}", Uuid::from_u128(0x64)),
        "",
    )
    .await;
    assert_eq!(body, "[]");
}

/// `RebuildDatabaseAsync` keeps only valid segments; the editor's create needs
/// `providerId`, its delete `itemId` + `type`; a Commercial delete without a
/// published match is a 404.
#[tokio::test]
async fn editor_and_rebuild_routes_validate_like_upstream() {
    let (_seg, app) = state();
    let item = Uuid::from_u128(0x52);
    app.intro_skipper
        .update_timestamp(stored(item, Mode::Recap, 0.0, 0.0))
        .await
        .unwrap();
    let (status, _) = send(app.clone(), "POST", "/Intros/RebuildDatabase", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(app.intro_skipper.segments(item).await.unwrap().is_empty());

    let body = r#"{"Type":"Intro","StartTicks":0,"EndTicks":10}"#;
    let (status, _) = send(
        app.clone(),
        "POST",
        &format!("/MediaSegmentsApi/{item}"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "providerId is [Required]");
    let segment = Uuid::from_u128(0x53);
    let (status, _) = send(
        app.clone(),
        "DELETE",
        &format!("/MediaSegmentsApi/{segment}"),
        "",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "itemId and type are [Required]"
    );
    let (status, _) = send(
        app.clone(),
        "DELETE",
        &format!("/MediaSegmentsApi/{segment}?itemId={item}&type=commercial"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        app,
        "DELETE",
        &format!("/MediaSegmentsApi/{segment}?itemId={item}&type=bogus"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Upstream binds `UpdateAnalyzerActionsRequest` through the MVC binder: an
/// object only, names and enum values (keys too) ignoring case.
#[tokio::test]
async fn analyzer_actions_bind_like_the_mvc_binder() {
    let tasks = || {
        Arc::new(MemTasks {
            running: false,
            started: Mutex::new(Vec::new()),
        })
    };
    let uri = "/Intros/AnalyzerActions/UpdateSeason";
    // `VisualizationController` is `RequiresElevation`: a plain user is
    // refused on both routes.
    let user_app = build_app_as(
        Arc::new(MemSegments::default()),
        tasks(),
        Arc::new(MemConfig::default()),
        Arc::new(AuthedAuthService),
    );
    let (status, _) = send(user_app.clone(), "POST", uri, r#"{"AnalyzerActions":{}}"#).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        user_app,
        "GET",
        &format!("/Intros/AnalyzerActions/{}", Uuid::nil()),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let app = build_app_as(
        Arc::new(MemSegments::default()),
        tasks(),
        Arc::new(MemConfig::default()),
        Arc::new(ferrofin_api::test_support::ApiKeyAuthService),
    );
    for body in [
        r#"{"id":"00000000-0000-0000-0000-000000000000","analyzerActions":{"introduction":"chromaprint","Credits":"None"}}"#,
        r#"{"Id":"00000000-0000-0000-0000-000000000000","AnalyzerActions":{"Recap":"BlackFrame"}}"#,
        // `JsonGuidConverter` reads null as the empty id.
        r#"{"Id":null,"AnalyzerActions":{}}"#,
        // The plugin UI's own request (web/src/store/api.ts, action-bar.ts).
        r#"{"id":"28c3ad34d0306759137254e7c81d74e0","analyzerActions":{"Recap":"Default","Introduction":"Chromaprint","Credits":"BlackFrame","Preview":"Chapter","Commercial":"None"}}"#,
    ] {
        let (status, _) = send(app.clone(), "POST", uri, body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    for body in ["[]", "5", r#"{"AnalyzerActions":{"Nope":"Default"}}"#] {
        let (status, _) = send(app.clone(), "POST", uri, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    // `SetAnalyzerActionAsync`: each named mode is replaced, the rest kept.
    let nil = app
        .intro_skipper
        .analyzer_actions(Uuid::nil())
        .await
        .unwrap();
    assert_eq!(nil.len(), 3);
    assert_eq!(nil[&Mode::Introduction], Action::Chromaprint);
    assert_eq!(nil[&Mode::Credits], Action::None);
    assert_eq!(nil[&Mode::Recap], Action::BlackFrame);
    let ui = Uuid::parse_str("28c3ad34d0306759137254e7c81d74e0").unwrap();
    let ui = app.intro_skipper.analyzer_actions(ui).await.unwrap();
    assert_eq!(ui[&Mode::Preview], Action::Chapter);
    assert_eq!(ui[&Mode::Commercial], Action::None);
}

/// `POST /FileTransformation/RegisterTransformation` registers a callback that
/// rewrites the JavaScript served to every browser, and each accepted
/// registration is retained for the life of the process in a registry nothing
/// sweeps. Upstream gates it with `Policies.RequiresElevation`; this port took a
/// bare `RequireAuth`, which let any authenticated account grow that registry —
/// measured at +157 MB of RssAnon over 150 requests carrying 1 MB of strings
/// each, linearly and with no plateau.
/// Upstream's plugin controllers are `RequiresElevation` (class-level on
/// `VisualizationController`, `SegmentEditorController` and
/// `TroubleshootingController`; per action on `SkipButtonCssController` and the
/// writes of `SkipIntroController`). Only the two episode reads are plain
/// `[Authorize]`.
#[tokio::test]
async fn plugin_routes_are_elevated_as_upstream() {
    let user_app = build_app_as(
        Arc::new(MemSegments::default()),
        Arc::new(MemTasks {
            running: false,
            started: Mutex::new(Vec::new()),
        }),
        Arc::new(MemConfig::default()),
        Arc::new(AuthedAuthService),
    );
    let id = Uuid::from_u128(1);
    for (method, uri) in [
        ("POST", format!("/Episode/{id}/Timestamps")),
        ("POST", "/Intros/EraseTimestamps".to_owned()),
        ("POST", "/Intros/RebuildDatabase".to_owned()),
        ("GET", "/MediaSegmentsApi".to_owned()),
        ("POST", format!("/MediaSegmentsApi/{id}")),
        ("DELETE", format!("/MediaSegmentsApi/{id}")),
        ("POST", "/SkipButtonCss/InjectCss".to_owned()),
        ("POST", "/SkipButtonCss/UpdateSkipDuration".to_owned()),
        ("GET", "/IntroSkipper".to_owned()),
        ("GET", "/IntroSkipper/SupportBundle".to_owned()),
        ("GET", format!("/Intros/AnalyzerActions/{id}")),
        ("POST", "/Intros/AnalyzerActions/UpdateSeason".to_owned()),
        ("GET", format!("/Intros/Show/{id}/{id}")),
        ("DELETE", format!("/Intros/Show/{id}/{id}")),
        ("POST", format!("/Intros/ScanSeason/{id}/{id}")),
        ("GET", "/Intros/ScanStatus".to_owned()),
        (
            "POST",
            "/FileTransformation/RegisterTransformation".to_owned(),
        ),
        ("GET", format!("/Intros/DisabledEpisodes/{id}")),
        ("POST", "/Intros/DisabledEpisodes/Update".to_owned()),
        ("DELETE", format!("/Intros/Show/{id}")),
        ("POST", "/Intros/ExcludedTimestamps/Clear".to_owned()),
    ] {
        let (status, _) = send(user_app.clone(), method, &uri, "{}").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
}

#[tokio::test]
async fn register_transformation_requires_an_administrator() {
    let seg = Arc::new(MemSegments::default());
    let tasks = Arc::new(MemTasks {
        running: false,
        started: Mutex::new(Vec::new()),
    });
    let config = Arc::new(MemConfig::default());

    // A plain authenticated user (no admin policy, not an API key) is refused.
    let user_app = build_app_as(
        seg.clone(),
        tasks.clone(),
        config.clone(),
        Arc::new(AuthedAuthService),
    );
    let (status, _) = send(
        user_app,
        "POST",
        "/FileTransformation/RegisterTransformation",
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An elevated caller still reaches the handler and gets upstream's `Ok()`.
    let admin_app = build_app_as(
        seg,
        tasks,
        config,
        Arc::new(ferrofin_api::test_support::ApiKeyAuthService),
    );
    let (status, _) = send(
        admin_app,
        "POST",
        "/FileTransformation/RegisterTransformation",
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn inject_css_writes_import_and_duration_then_updates() {
    let seg = Arc::new(MemSegments::default());
    let config = Arc::new(MemConfig::default());
    let app = build_app(
        seg,
        Arc::new(MemTasks {
            running: false,
            started: Mutex::new(Vec::new()),
        }),
        config.clone(),
    );

    // Update-only with no prior injection is a no-op success (nothing to update).
    let (status, _) = send(app.clone(), "POST", "/SkipButtonCss/UpdateSkipDuration", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(config.branding.lock().unwrap().custom_css.is_none());

    // Inject writes the import + the duration variable (from config's delay=11).
    let (status, _) = send(app.clone(), "POST", "/SkipButtonCss/InjectCss", "").await;
    assert_eq!(status, StatusCode::OK);
    let css = config.branding.lock().unwrap().custom_css.clone().unwrap();
    assert!(css.contains("intro-skipper-css"), "import injected");
    assert!(
        css.contains("--skip-hide-duration: 11s;"),
        "duration from plugin config"
    );

    // A second inject is idempotent (import already present).
    let (status, _) = send(app, "POST", "/SkipButtonCss/InjectCss", "").await;
    assert_eq!(status, StatusCode::OK);
    let css2 = config.branding.lock().unwrap().custom_css.clone().unwrap();
    assert_eq!(css2.matches("intro-skipper-css").count(), 1);
}
