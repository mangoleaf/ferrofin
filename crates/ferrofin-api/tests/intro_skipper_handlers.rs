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
    build_app_as(segments, tasks, config, Arc::new(AuthedAuthService))
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

#[tokio::test]
async fn scan_status_reports_idle_and_running() {
    let (_seg, idle) = state();
    let (status, body) = send(idle, "GET", "/Intros/ScanStatus", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"isRunning":false}"#);

    let busy = build_app(
        Arc::new(MemSegments::default()),
        Arc::new(MemTasks {
            running: true,
            started: Mutex::new(Vec::new()),
        }),
        Arc::new(MemConfig::default()),
    );
    let (_status, body) = send(busy, "GET", "/Intros/ScanStatus", "").await;
    assert_eq!(body, r#"{"isRunning":true}"#);
}

#[tokio::test]
async fn scan_season_starts_task_or_conflicts() {
    // Idle → 202 and the detection task is started.
    let started = Arc::new(MemTasks {
        running: false,
        started: Mutex::new(Vec::new()),
    });
    let app = build_app(
        Arc::new(MemSegments::default()),
        started.clone(),
        Arc::new(MemConfig::default()),
    );
    let series = Uuid::new_v4();
    let season = Uuid::new_v4();
    let (status, _) = send(
        app,
        "POST",
        &format!("/Intros/ScanSeason/{series}/{season}"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // The handler addresses the task by its WIRE id (`ScheduledTaskWorker.Id`),
    // not by its key — that is what `ITaskManager`'s lookup takes.
    assert_eq!(
        started.started.lock().unwrap().as_slice(),
        [ferrofin_traits::tasks::task_id_for_key(
            "IntroSkipper.Detect"
        )]
    );

    // Already running → 409.
    let busy = build_app(
        Arc::new(MemSegments::default()),
        Arc::new(MemTasks {
            running: true,
            started: Mutex::new(Vec::new()),
        }),
        Arc::new(MemConfig::default()),
    );
    let (status, _) = send(
        busy,
        "POST",
        &format!("/Intros/ScanSeason/{series}/{season}"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn erase_timestamps_deletes_matching_provider_rows() {
    let (seg, app) = state();
    let item = Uuid::new_v4();
    // Two IntroSkipper rows (Intro + Outro) and one other-provider row.
    seg.rows.lock().unwrap().push((
        "IntroSkipper".to_owned(),
        MediaSegmentDto {
            id: Uuid::new_v4(),
            item_id: item,
            type_: MediaSegmentType::Intro,
            start_ticks: 0,
            end_ticks: 1,
        },
    ));
    seg.rows.lock().unwrap().push((
        "IntroSkipper".to_owned(),
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

    let (status, _) = send(app, "POST", "/Intros/EraseTimestamps?mode=Introduction", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let rows = seg.rows.lock().unwrap();
    // The IntroSkipper Intro row is gone; the Outro and the other provider stay.
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|(p, s)| !(p == "IntroSkipper" && s.type_ == MediaSegmentType::Intro))
    );
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
    for (method, uri, body) in [
        ("POST", "/Intros/RebuildDatabase", ""),
        (
            "POST",
            "/Intros/AnalyzerActions/UpdateSeason",
            r#"{"Id":"00000000-0000-0000-0000-000000000000","AnalyzerActions":{}}"#,
        ),
    ] {
        let (status, _) = send(app.clone(), method, uri, body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{method} {uri}");
    }
}

/// `POST /FileTransformation/RegisterTransformation` registers a callback that
/// rewrites the JavaScript served to every browser, and each accepted
/// registration is retained for the life of the process in a registry nothing
/// sweeps. Upstream gates it with `Policies.RequiresElevation`; this port took a
/// bare `RequireAuth`, which let any authenticated account grow that registry —
/// measured at +157 MB of RssAnon over 150 requests carrying 1 MB of strings
/// each, linearly and with no plateau.
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
