//! Composition root — assembles every concrete `hermit-core` manager into a
//! `hermit_api::AppState`.
//!
//! Port of the Autofac service-registration in `Jellyfin.Server`'s `Startup`
//! plus `CoreAppHost.RegisterServices`: it constructs the managers in strict
//! dependency order (leaf repositories first, then the services that consume
//! them, then the managers that consume those services) and injects them as the
//! `Arc<dyn Trait>` fields of [`AppState`].
//!
//! The construction order is the topological order of the manager dependency
//! DAG; see `brain/PLAN_HERMIT_PORT.md`. This unit wires the 33 core managers
//! and then replaces the media-encoding seams (`hls` / `attachments`) — installed
//! as disabled stubs by [`AppState::new`] — with the real ffmpeg-backed transcode
//! pair via [`with_media_encoding`](AppState::with_media_encoding) (built by
//! [`build_media_encoding`](crate::media_encoding::build_media_encoding)).

use std::sync::Arc;

use anyhow::Context as _;
use hermit_api::AppState;
use hermit_core::application_host::HostNetworkInfo;
use hermit_core::system_manager::{LifecycleController, SystemHostFacts};
use hermit_core::{
    HermitActivityManager, HermitApiKeyManager, HermitAuthService, HermitAuthorizationContext,
    HermitChapterManager, HermitChapterRepository, HermitClientEventLogger,
    HermitCollectionManager, HermitDeviceManager, HermitDisplayPreferencesManager,
    HermitDtoService, HermitEventManager, HermitExternalDataManager, HermitFileSystem,
    HermitItemCountService, HermitItemPersistenceService, HermitItemRepository,
    HermitKeyframeRepository, HermitLibraryManager, HermitLinkedChildrenService,
    HermitLyricManager, HermitMediaAttachmentRepository, HermitMediaSegmentManager,
    HermitMediaSourceManager, HermitMediaStreamRepository, HermitMusicManager, HermitNextUpService,
    HermitPathManager, HermitPeopleRepository, HermitPlaylistManager, HermitQuickConnect,
    HermitSearchManager, HermitServerApplicationHost, HermitServerConfigurationManager,
    HermitSessionManager, HermitSimilarItemsManager, HermitSubtitleManager, HermitSystemManager,
    HermitTaskManager, HermitTrickplayManager, HermitTvSeriesManager, HermitUserDataManager,
    HermitUserManager, HermitUserViewManager, ItemTypeLookup, LocalizationManager,
};
use hermit_db::Database;
use hermit_drawing::{ImageProcessor, NullImageEncoder};
use hermit_livetv::DisabledLiveTvManager;
use hermit_mediaencoding::{MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder};
use hermit_providers::LocalProviderManager;
use hermit_traits::system::ServerApplicationPaths as _;

use crate::bootstrap::FfmpegPaths;
use crate::config::Config;
use crate::media_encoding::build_media_encoding;

/// The product / package identity advertised by the server, ported from
/// Jellyfin's `ApplicationHost` constants.
const PRODUCT_NAME: &str = "Hermit Server";

/// The package name reported in system info (`IStartupOptions.PackageName`).
const PACKAGE_NAME: &str = "hermit-server";

/// The **Jellyfin server version Hermit reports** in `SystemInfo`/`PublicSystemInfo`
/// (`Version`). This is the version the vendored OpenAPI contract targets — i.e.
/// "Hermit speaks Jellyfin 10.11.8's API" — NOT Hermit's own crate version
/// (`CARGO_PKG_VERSION`, used only for build/log lines). Clients gate on it:
/// jellyfin-web's SDK refuses any server below `MINIMUM_VERSION = 10.10.0` with an
/// "Update Required" screen, so reporting Hermit's `0.1.0` locks the web client out.
/// Keep this in sync with `contracts/jellyfin-openapi-*.json`.
const JELLYFIN_API_VERSION: &str = "10.11.8";

/// The assembled application state plus the handles the composition root still
/// needs after wiring (the concrete host, to flip its startup flag and drive
/// name refresh, and the lifecycle controller's restart flag).
pub struct WiredApp {
    /// The fully-wired shared state handed to every axum handler.
    pub state: AppState,
    /// The concrete host — the composition root calls
    /// [`HermitServerApplicationHost::mark_core_startup_complete`] on it once
    /// the router is mounted, mirroring `CoreAppHost`'s post-startup flag.
    pub app_host: Arc<HermitServerApplicationHost>,
}

/// The concrete [`LifecycleController`] for the running server.
///
/// Port of the slice of `IHostApplicationLifetime` the system manager drives:
/// `stop(restart)` records whether a restart was requested and signals the axum
/// graceful-shutdown handle; the `has_pending_restart` / `is_shutting_down`
/// flags mirror `IServerApplicationHost.HasPendingRestart` / the host's
/// shutdown state. Only a test `Fake` existed before this unit.
pub struct HermitLifecycleController {
    /// Fires the axum `with_graceful_shutdown` future; taken on the first stop.
    shutdown: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Set once a stop has been requested (the server is winding down).
    shutting_down: std::sync::atomic::AtomicBool,
    /// Set when the requested stop should be followed by a restart.
    restart_pending: std::sync::atomic::AtomicBool,
}

impl HermitLifecycleController {
    /// Builds the controller over the axum graceful-shutdown trigger.
    #[must_use]
    pub fn new(shutdown: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            shutdown: tokio::sync::Mutex::new(Some(shutdown)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            restart_pending: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl LifecycleController for HermitLifecycleController {
    async fn stop(&self, restart: bool) -> Result<(), hermit_traits::error::ServiceError> {
        use std::sync::atomic::Ordering;
        self.restart_pending.store(restart, Ordering::SeqCst);
        self.shutting_down.store(true, Ordering::SeqCst);
        // Fire the graceful-shutdown trigger once; a second stop is a no-op.
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }
        Ok(())
    }

    fn has_pending_restart(&self) -> bool {
        self.restart_pending
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Assembles every concrete manager over `db` + `config` and returns the wired
/// [`AppState`].
///
/// The `ffmpeg` paths seed the (probe-only) media encoder; pass the discovered
/// paths, or [`FfmpegPaths`] pointing at bare `ffmpeg`/`ffprobe` when discovery
/// failed (playback then 500s until a working ffmpeg is configured). The
/// `shutdown` sender is handed to the [`HermitLifecycleController`] so a
/// `/System/Restart|Shutdown` request can trigger axum's graceful shutdown.
///
/// Managers are constructed in dependency order: leaf repositories and the
/// `Database`-only services first, then the managers that consume them, then the
/// managers that consume *those*. The media-encoding seams are left as the
/// disabled stubs — the transcode pair is injected by a later unit.
///
/// # Errors
///
/// Returns an error if the configuration manager fails to load its persisted
/// `system.json`, or if refreshing the advertised server name fails.
// The composition root is one long linear wiring sequence: every manager is
// constructed exactly once, in dependency order, and handed to `AppState::new`.
// Splitting it would only scatter the topological order across helpers that each
// take (and re-thread) most of the same collaborators, so the single-function
// form is the clearest expression of the DAG.
#[allow(clippy::too_many_lines)]
pub async fn build_app_state(
    db: &Database,
    config: &Config,
    ffmpeg: &FfmpegPaths,
    shutdown: tokio::sync::oneshot::Sender<()>,
) -> anyhow::Result<WiredApp> {
    // ---- paths (concrete) -------------------------------------------------
    // Sub-directory layout under the program-data root, ported from
    // `ServerApplicationPaths`: {data}/log, {config}, {cache}, {web}.
    let paths = Arc::new(hermit_core::HermitServerApplicationPaths::new(
        &config.data_dir,
        config.data_dir.join("log"),
        &config.config_dir,
        &config.cache_dir,
        &config.web_dir,
    ));

    // ---- configuration manager (loads persisted system.json) --------------
    let config_mgr = Arc::new(
        HermitServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .context("failed to load server configuration")?,
    );
    let server_config = config_mgr.snapshot();

    // A stable server/system id, persisted across restarts. Jellyfin persists its
    // SystemId and the web client keys stored sessions by it, so regenerating it on
    // every boot breaks reconnect: `ServerConnections.getApiClient` throws on the
    // now-unknown id and the UI dies with a black screen. Persist to a dedicated
    // `{config}/system_id` file.
    // ponytail: a plain id file, not a new field threaded through the config manager.
    let server_id = {
        let id_path = config.config_dir.join("system_id");
        match tokio::fs::read_to_string(&id_path).await {
            Ok(s) if !s.trim().is_empty() => s.trim().to_owned(),
            _ => {
                let id = uuid::Uuid::new_v4().simple().to_string();
                tokio::fs::create_dir_all(&config.config_dir).await.ok();
                tokio::fs::write(&id_path, &id)
                    .await
                    .context("failed to persist system_id")?;
                id
            }
        }
    };

    // ---- leaf: lookup + repositories (Database-only) ----------------------
    let item_type_lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    let item_repository: Arc<dyn hermit_traits::persistence::ItemRepository> = Arc::new(
        HermitItemRepository::new(db.clone(), Arc::clone(&item_type_lookup)),
    );
    let item_count_service: Arc<dyn hermit_traits::persistence::ItemCountService> =
        Arc::new(HermitItemCountService::new(db.clone()));
    let item_persistence_service: Arc<dyn hermit_traits::persistence::ItemPersistenceService> =
        Arc::new(HermitItemPersistenceService::new(db.clone()));
    let people_repository: Arc<dyn hermit_traits::persistence::PeopleRepository> =
        Arc::new(HermitPeopleRepository::new(db.clone()));
    let media_stream_repository: Arc<dyn hermit_traits::persistence::MediaStreamRepository> =
        Arc::new(HermitMediaStreamRepository::new(db.clone()));
    let media_attachment_repository: Arc<
        dyn hermit_traits::persistence::MediaAttachmentRepository,
    > = Arc::new(HermitMediaAttachmentRepository::new(db.clone()));
    let chapter_repository: Arc<dyn hermit_traits::persistence::ChapterRepository> =
        Arc::new(HermitChapterRepository::new(db.clone()));
    let keyframe_repository: Arc<dyn hermit_traits::persistence::KeyframeRepository> =
        Arc::new(HermitKeyframeRepository::new(db.clone()));
    let linked_children_service: Arc<dyn hermit_traits::persistence::LinkedChildrenService> =
        Arc::new(HermitLinkedChildrenService::new(db.clone()));
    let next_up_service: Arc<dyn hermit_traits::persistence::NextUpService> =
        Arc::new(HermitNextUpService::new(db.clone()));

    // ---- standalone leaves (no manager deps) ------------------------------
    let image_processor: Arc<dyn hermit_traits::drawing::ImageProcessor> = Arc::new(
        ImageProcessor::new(Arc::new(NullImageEncoder::new()), paths.image_cache_path()),
    );
    let providers: Arc<dyn hermit_traits::providers::ProviderManager> =
        Arc::new(LocalProviderManager::new(Vec::new()));
    let file_system: Arc<dyn hermit_traits::filesystem::FileSystem> =
        Arc::new(HermitFileSystem::new());
    let event_manager: Arc<dyn hermit_traits::events::EventManager> =
        Arc::new(HermitEventManager::new());
    let localization: Arc<dyn hermit_traits::localization::LocalizationManager> = Arc::new(
        LocalizationManager::new(&server_config.metadata_country_code),
    );
    let lyrics: Arc<dyn hermit_traits::stubs::LyricManager> = Arc::new(HermitLyricManager::new());
    let _live_tv: Arc<dyn hermit_traits::stubs::LiveTvManager> = Arc::new(DisabledLiveTvManager);
    let path_manager: Arc<dyn hermit_traits::system::PathManager> =
        Arc::new(HermitPathManager::new(Arc::clone(&paths)));
    let client_event_logger: Arc<dyn hermit_traits::events::ClientEventLogger> =
        Arc::new(HermitClientEventLogger::new(
            Arc::clone(&paths) as Arc<dyn hermit_traits::system::ServerApplicationPaths>
        ));

    // ---- media encoder (probe-only; ffmpeg process seam) ------------------
    let media_encoder: Arc<dyn hermit_traits::media_encoding::MediaEncoder> =
        Arc::new(MediaEncoderImpl::new(
            Arc::new(TokioTranscoder::new()),
            ffmpeg.ffmpeg.to_string_lossy().into_owned(),
            ffmpeg.ffprobe.to_string_lossy().into_owned(),
            MediaEncoderConfig {
                analyze_duration: None,
                probe_size: None,
                threads: 0,
            },
        ));

    // ---- config trait object (shared by many managers) --------------------
    let config_trait: Arc<dyn hermit_traits::configuration::ServerConfigurationManager> =
        Arc::clone(&config_mgr) as Arc<_>;

    // ---- managers over repositories/services ------------------------------
    let users: Arc<dyn hermit_traits::library::UserManager> =
        Arc::new(HermitUserManager::new(db.clone()).with_server_id(server_id.clone()));
    let user_data: Arc<dyn hermit_traits::library::UserDataManager> = Arc::new(
        HermitUserDataManager::new(db.clone(), Arc::clone(&config_trait)),
    );
    let devices: Arc<dyn hermit_traits::devices::DeviceManager> =
        Arc::new(HermitDeviceManager::new(db.clone()));
    let api_keys: Arc<dyn hermit_traits::security::ApiKeyManager> =
        Arc::new(HermitApiKeyManager::new(db.clone()));
    let display_preferences: Arc<dyn hermit_traits::configuration::DisplayPreferencesManager> =
        Arc::new(HermitDisplayPreferencesManager::new(db.clone()));
    let activity: Arc<dyn hermit_traits::activity::ActivityManager> =
        Arc::new(HermitActivityManager::new(db.clone()));
    let user_views: Arc<dyn hermit_traits::library::UserViewManager> =
        Arc::new(HermitUserViewManager::new(Arc::clone(&item_repository)));
    let music: Arc<dyn hermit_traits::library::MusicManager> =
        Arc::new(HermitMusicManager::new(Arc::clone(&item_repository)));
    let similar_items: Arc<dyn hermit_traits::library::SimilarItemsManager> =
        Arc::new(HermitSimilarItemsManager::new(Arc::clone(&item_repository)));
    let search: Arc<dyn hermit_traits::library::SearchManager> =
        Arc::new(HermitSearchManager::new(Arc::clone(&item_repository)));
    let trickplay: Arc<dyn hermit_traits::trickplay::TrickplayManager> = Arc::new(
        HermitTrickplayManager::new(db.clone(), Arc::clone(&path_manager)),
    );

    // ---- library + media-sources (consume repositories/services) ----------
    // The virtual-folder manager (shared with `with_virtual_folders` below) and
    // the filesystem scanner the library manager runs on `queue_library_scan`.
    let virtual_folders: Arc<dyn hermit_traits::library::VirtualFolderManager> = Arc::new(
        hermit_core::HermitVirtualFolderManager::new(paths.default_user_views_path())
            .with_item_store(Arc::clone(&item_persistence_service)),
    );
    let library_scanner = Arc::new(hermit_core::LibraryScanner::new(
        Arc::clone(&virtual_folders),
        Arc::clone(&file_system),
        Arc::clone(&item_persistence_service),
    ));
    let library: Arc<dyn hermit_traits::library::LibraryManager> = Arc::new(
        HermitLibraryManager::new(
            Arc::clone(&item_repository),
            Arc::clone(&item_count_service),
            Arc::clone(&item_persistence_service),
            Arc::clone(&people_repository),
        )
        .with_scanner(Arc::clone(&library_scanner)),
    );
    // Scheduled tasks: register the "Scan all libraries" task (drives the same
    // scan as `POST /Library/Refresh`) so the dashboard button + tasks page work.
    let task_manager = HermitTaskManager::new();
    task_manager.register(Arc::new(hermit_core::RefreshLibraryTask::new(Arc::clone(
        &library,
    ))));
    let tasks: Arc<dyn hermit_traits::tasks::TaskManager> = Arc::new(task_manager);
    let media_sources: Arc<dyn hermit_traits::library::MediaSourceManager> =
        Arc::new(HermitMediaSourceManager::new(
            Arc::clone(&item_repository),
            Arc::clone(&media_stream_repository),
            Arc::clone(&media_attachment_repository),
            Arc::clone(&media_encoder),
            Arc::clone(&providers),
        ));

    // ---- managers over library -------------------------------------------
    let chapters: Arc<dyn hermit_traits::chapters::ChapterManager> = Arc::new(
        HermitChapterManager::new(Arc::clone(&chapter_repository), Arc::clone(&library)),
    );
    let subtitles: Arc<dyn hermit_traits::subtitles::SubtitleManager> =
        Arc::new(HermitSubtitleManager::new(db.clone(), Arc::clone(&library)));
    let media_segments: Arc<dyn hermit_traits::media_segments::MediaSegmentManager> = Arc::new(
        HermitMediaSegmentManager::new(db.clone(), Arc::clone(&library)),
    );
    let collections: Arc<dyn hermit_traits::collections::CollectionManager> =
        Arc::new(HermitCollectionManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&linked_children_service),
        ));
    let playlists: Arc<dyn hermit_traits::collections::PlaylistManager> =
        Arc::new(HermitPlaylistManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&linked_children_service),
        ));
    let _external_data: Arc<dyn hermit_traits::system::ExternalDataManager> =
        Arc::new(HermitExternalDataManager::new(
            Arc::clone(&path_manager),
            Arc::clone(&keyframe_repository),
            Arc::clone(&media_segments),
            Arc::clone(&trickplay),
            Arc::clone(&chapters),
        ));

    // ---- dto (consumes many of the above) ---------------------------------
    let dto: Arc<dyn hermit_traits::dto::DtoService> = Arc::new(HermitDtoService::new(
        db.clone(),
        server_id.clone(),
        Arc::clone(&library),
        Arc::clone(&user_data),
        Arc::clone(&item_count_service),
        Arc::clone(&image_processor),
        Arc::clone(&media_sources),
        Arc::clone(&chapters),
        Arc::clone(&trickplay),
        Arc::clone(&providers),
    ));

    // ---- sessions + tv_series (consume dto) -------------------------------
    let sessions: Arc<dyn hermit_traits::session::SessionManager> =
        Arc::new(HermitSessionManager::new(
            Arc::clone(&users),
            Arc::clone(&devices),
            Arc::clone(&user_data),
            Arc::clone(&library),
            Arc::clone(&dto),
            Arc::clone(&event_manager),
            db.clone(),
            server_id.clone(),
        ));
    let tv_series: Arc<dyn hermit_traits::tv::TvSeriesManager> =
        Arc::new(HermitTvSeriesManager::new(
            Arc::clone(&users),
            Arc::clone(&library),
            Arc::clone(&next_up_service),
            Arc::clone(&dto),
            Arc::clone(&config_trait),
        ));

    // ---- host + system + auth + quick-connect -----------------------------
    let app_host = Arc::new(HermitServerApplicationHost::new(
        Arc::clone(&paths),
        Arc::clone(&config_trait),
        HostNetworkInfo {
            http_port: config.port,
            https_port: config.https_port,
            listen_with_https: false,
            published_server_url: config.published_url.clone(),
            base_url: config.base_url.clone(),
            enable_published_server_uri_by_request: false,
        },
        config.server_name.clone(),
    ));
    app_host
        .refresh_server_name()
        .await
        .context("failed to refresh advertised server name")?;
    let app_host_trait: Arc<dyn hermit_traits::system::ServerApplicationHost> =
        Arc::clone(&app_host) as Arc<_>;

    let lifecycle: Arc<dyn LifecycleController> =
        Arc::new(HermitLifecycleController::new(shutdown));
    let system: Arc<dyn hermit_traits::system::SystemManager> = Arc::new(HermitSystemManager::new(
        Arc::clone(&app_host_trait),
        Arc::clone(&config_trait),
        Arc::clone(&paths),
        Arc::clone(&lifecycle),
        SystemHostFacts {
            // Report the emulated Jellyfin API version (clients gate on this), not
            // Hermit's own crate version — see JELLYFIN_API_VERSION.
            version: Some(JELLYFIN_API_VERSION.to_owned()),
            product_name: Some(PRODUCT_NAME.to_owned()),
            system_id: Some(server_id.clone()),
            package_name: Some(PACKAGE_NAME.to_owned()),
            transcoding_temp_path: None,
            completed_installations: Vec::new(),
        },
    ));

    // The auth service wraps an owned concrete authorization context, so build
    // that concrete value, clone it into the service, and box the other for the
    // `auth_context` slot.
    let auth_context_concrete = HermitAuthorizationContext::new(
        db.clone(),
        Arc::clone(&users),
        Arc::clone(&app_host_trait),
        Arc::clone(&config_trait),
        server_id.clone(),
        env!("CARGO_PKG_VERSION").to_owned(),
    );
    let auth_service: Arc<dyn hermit_traits::net::AuthService> =
        Arc::new(HermitAuthService::new(auth_context_concrete.clone()));
    let auth_context: Arc<dyn hermit_traits::net::AuthorizationContext> =
        Arc::new(auth_context_concrete);

    let quick_connect: Arc<dyn hermit_traits::security::QuickConnect> = Arc::new(
        HermitQuickConnect::new(Arc::clone(&config_trait), Arc::clone(&sessions)),
    );

    // Clone the collaborators the media-encoding pair needs before they are
    // moved into `AppState::new` below.
    let me_media_sources = Arc::clone(&media_sources);
    let me_media_encoder = Arc::clone(&media_encoder);
    let me_config = Arc::clone(&config_trait);
    let me_path_manager = Arc::clone(&path_manager);

    // ---- assemble (33 managers, in AppState::new field order) -------------
    let state = AppState::new(
        library,
        users,
        user_views,
        user_data,
        media_sources,
        sessions,
        system,
        app_host_trait,
        config_trait,
        providers,
        music,
        similar_items,
        search,
        dto,
        auth_context,
        auth_service,
        quick_connect,
        playlists,
        collections,
        tv_series,
        subtitles,
        lyrics,
        media_segments,
        trickplay,
        devices,
        client_event_logger,
        api_keys,
        localization,
        display_preferences,
        activity,
        file_system,
        tasks,
    );

    // ---- media-encoding pair (real transcode/HLS + attachments) -----------
    // Replace the disabled stubs `AppState::new` installed with the
    // ffmpeg-backed transcode/HLS chain and the real attachment extractor.
    // Called before the state is cloned/shared (mirrors the C# Autofac
    // registration of ITranscodeManager/IDynamicHlsPlaylistGenerator +
    // IAttachmentExtractor).
    let (hls, attachments) = build_media_encoding(
        me_media_sources,
        me_media_encoder,
        me_config,
        Arc::clone(&paths),
        me_path_manager,
    );
    let state = state.with_media_encoding(hls, attachments);

    // ---- virtual-folder (library-structure) store -------------------------
    // The `/Library/VirtualFolders*` + `/Library/PhysicalPaths` admin surface is
    // filesystem-backed: each library is a directory under `DefaultUserViewsPath`
    // (`.mblink` shortcuts + `options.json`). Replace the disabled stub
    // `AppState::new` installed with the real filesystem-backed manager rooted
    // there (mirrors the C# `ILibraryManager` virtual-folder methods).
    let state = state.with_virtual_folders(Arc::clone(&virtual_folders));

    // ---- plugin manager (Tier 1: compile-time plugins) --------------------
    // Backs `/Plugins/*`, `/Packages/*`, and `/Repositories` over the compile-time
    // plugin registry. No plugins are registered yet, so the plugin list is empty;
    // the manager still persists the repository list and per-plugin configuration
    // under `{config}/plugins/`. Runtime install/load is Tier 2 (a WASM/libloading
    // host). See brain/PLAN_HERMIT_PLUGINS.md.
    let state = state.with_plugins(Arc::new(hermit_core::HermitPluginManager::empty(
        config.config_dir.join("plugins"),
    )));

    Ok(WiredApp { state, app_host })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A bootstrap [`Config`] pointing every path at a fresh temp dir.
    fn test_config(root: &std::path::Path) -> Config {
        Config {
            data_dir: root.join("data"),
            config_dir: root.join("config"),
            cache_dir: root.join("cache"),
            web_dir: root.join("web"),
            bind_addr: "127.0.0.1".parse().unwrap(),
            port: 8096,
            https_port: 8920,
            published_url: None,
            base_url: String::new(),
            ffmpeg_path: None,
            ffprobe_path: None,
            library_roots: Vec::new(),
            server_name: "hermit-test".to_owned(),
            log_level: "info".to_owned(),
            admin_user: "admin".to_owned(),
            admin_password: String::new(),
        }
    }

    #[tokio::test]
    async fn build_app_state_wires_over_in_memory_db() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        let config = test_config(tmp.path());
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory db opens");
        db.run_migrations().await.expect("migrations apply");

        let ffmpeg = FfmpegPaths {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
        };
        let (tx, _rx) = tokio::sync::oneshot::channel();

        let wired = build_app_state(&db, &config, &ffmpeg, tx)
            .await
            .expect("app state wires");

        // The router builds over the wired state without panicking (every
        // manager slot is populated).
        let _router = hermit_api::create_router(wired.state.clone());
        // The host starts un-flagged; the composition root flips it after mount.
        wired.app_host.mark_core_startup_complete();
    }

    #[tokio::test]
    async fn lifecycle_controller_signals_shutdown_and_flags_restart() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let lifecycle = HermitLifecycleController::new(tx);
        assert!(!lifecycle.is_shutting_down());
        assert!(!lifecycle.has_pending_restart());

        lifecycle.stop(true).await.unwrap();
        assert!(lifecycle.is_shutting_down());
        assert!(lifecycle.has_pending_restart());
        // The graceful-shutdown trigger fired.
        assert!(rx.await.is_ok());

        // A second stop is a no-op (trigger already taken) and stays consistent.
        lifecycle.stop(false).await.unwrap();
        assert!(!lifecycle.has_pending_restart());
    }
}
