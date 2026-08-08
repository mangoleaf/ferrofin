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
use hermit_drawing::{ImageCrateEncoder, ImageProcessor};
use hermit_livetv::HermitLiveTvManager;
use hermit_mediaencoding::{
    MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder, TrickplayFrameExtractorImpl,
};
use hermit_providers::LocalProviderManager;
use hermit_traits::system::ServerApplicationPaths as _;

use crate::bootstrap::FfmpegPaths;
use crate::config::Config;
use crate::media_encoding::build_media_encoding;

/// The product / package identity advertised by the server, ported from
/// Jellyfin's `ApplicationHost` constants.
const PRODUCT_NAME: &str = "Jellyfin Server";

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
    /// The web-file transformation pipeline, shared with the static `/web`
    /// mount so registered transformations apply to the served files.
    pub file_transformations: Arc<dyn hermit_traits::plugins::FileTransformationService>,
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

/// Adapts the [`VirtualFolderManager`](hermit_traits::library::VirtualFolderManager)
/// to the system manager's [`LibraryStorageProvider`] seam, so the storage page
/// reports each library folder's real disk usage.
struct VirtualFolderStorage(Arc<dyn hermit_traits::library::VirtualFolderManager>);

#[async_trait::async_trait]
impl hermit_core::system_manager::LibraryStorageProvider for VirtualFolderStorage {
    async fn libraries(&self) -> Vec<(uuid::Uuid, String, Vec<String>)> {
        self.0
            .get_virtual_folders()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|vf| {
                let id = vf
                    .item_id
                    .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                    .unwrap_or_default();
                (id, vf.name.unwrap_or_default(), vf.locations)
            })
            .collect()
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
    // The real `image`-crate encoder (resize + format-convert), not the no-op
    // `NullImageEncoder` — so `maxWidth`/`fillHeight`/`format` image requests are
    // honoured instead of serving the full-size original.
    let image_processor: Arc<dyn hermit_traits::drawing::ImageProcessor> = Arc::new(
        ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), paths.image_cache_path()),
    );
    // The shared TMDB client — the scan's automatic artwork, the remote-search
    // ("Identify") providers, and the remote-image ("Choose Image") methods all
    // use Jellyfin's built-in key.
    let tmdb_client = Arc::new(hermit_providers::TmdbClient::new());
    let metadata_library = std::path::PathBuf::from(paths.internal_metadata_path()).join("library");
    // TheTVDB — the TV authority. Ships on with the built-in project key (like
    // TMDB); a user key/PIN override enables their subscription tier.
    let the_tvdb = Arc::new(hermit_providers::TvdbClient::with_config(
        &config.tvdb_api_key,
        &config.tvdb_subscriber_pin,
    ));
    let search_providers: Vec<Arc<dyn hermit_providers::RemoteSearchProvider>> = vec![
        Arc::new(hermit_providers::TmdbSearchProvider::new(
            Arc::clone(&tmdb_client),
            hermit_providers::TmdbKind::Movie,
        )),
        Arc::new(hermit_providers::TmdbSearchProvider::new(
            Arc::clone(&tmdb_client),
            hermit_providers::TmdbKind::Series,
        )),
        Arc::new(hermit_providers::TvdbSearchProvider::new(Arc::clone(
            &the_tvdb,
        ))),
    ];
    // Studio logos from the artwork repository (name-matched, keyless). The repo
    // URL is overridable; empty falls back to the built-in emby-artwork tree.
    let studios_client = Arc::new(hermit_providers::StudiosClient::with_repo_url(
        &config.studios_repo_url,
    ));
    let providers: Arc<dyn hermit_traits::providers::ProviderManager> = Arc::new(
        LocalProviderManager::new(Vec::new())
            .with_image_store(
                Arc::clone(&item_persistence_service),
                metadata_library.clone(),
            )
            .with_remote_images(Arc::clone(&tmdb_client), Arc::clone(&item_repository))
            .with_remote_search_providers(search_providers)
            .with_studios(studios_client),
    );
    let file_system: Arc<dyn hermit_traits::filesystem::FileSystem> =
        Arc::new(HermitFileSystem::new());
    let event_manager: Arc<dyn hermit_traits::events::EventManager> =
        Arc::new(HermitEventManager::new());
    let localization: Arc<dyn hermit_traits::localization::LocalizationManager> = Arc::new(
        LocalizationManager::new(&server_config.metadata_country_code),
    );
    let lyric_providers: Vec<Arc<dyn hermit_traits::stubs::LyricProvider>> =
        vec![Arc::new(hermit_providers::LrcLibProvider::new())];
    let lyrics: Arc<dyn hermit_traits::stubs::LyricManager> = Arc::new(
        HermitLyricManager::new()
            .with_items(Arc::clone(&item_repository))
            .with_providers(lyric_providers),
    );
    let live_tv: Arc<dyn hermit_traits::stubs::LiveTvManager> = Arc::new(HermitLiveTvManager::new(
        db.clone(),
        Arc::new(hermit_livetv::ReqwestFetcher::new()),
        server_id.clone(),
    ));
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
    // ONE auth cache shared by the authorization context (read-through) and the
    // user/device managers (invalidation on mutation) — the sharing is what
    // makes cached auth revocation-correct. See hermit_core::auth_cache.
    let auth_cache = Arc::new(hermit_core::auth_cache::AuthCache::default());
    let users: Arc<dyn hermit_traits::library::UserManager> = Arc::new(
        HermitUserManager::new(db.clone())
            .with_server_id(server_id.clone())
            .with_profile_image_dir(
                std::path::PathBuf::from(paths.internal_metadata_path()).join("users"),
            )
            .with_auth_cache(Arc::clone(&auth_cache)),
    );
    let user_data: Arc<dyn hermit_traits::library::UserDataManager> = Arc::new(
        HermitUserDataManager::new(db.clone(), Arc::clone(&config_trait)),
    );
    let devices: Arc<dyn hermit_traits::devices::DeviceManager> =
        Arc::new(HermitDeviceManager::new(db.clone()).with_auth_cache(Arc::clone(&auth_cache)));
    let api_keys: Arc<dyn hermit_traits::security::ApiKeyManager> =
        Arc::new(HermitApiKeyManager::new(db.clone()));
    let display_preferences: Arc<dyn hermit_traits::configuration::DisplayPreferencesManager> =
        Arc::new(HermitDisplayPreferencesManager::new(db.clone()));
    let activity: Arc<dyn hermit_traits::activity::ActivityManager> =
        Arc::new(HermitActivityManager::new(db.clone()));
    // The playlists media folder lives at `{data}/playlists` (C#
    // `ManualPlaylistsFolder`); the user-view seam provisions it lazily.
    let playlists_path = std::path::PathBuf::from(paths.data_path()).join("playlists");
    let user_views: Arc<dyn hermit_traits::library::UserViewManager> = Arc::new(
        HermitUserViewManager::new(Arc::clone(&item_repository))
            .with_playlists_store(Arc::clone(&item_persistence_service), playlists_path),
    );
    let music: Arc<dyn hermit_traits::library::MusicManager> =
        Arc::new(HermitMusicManager::new(Arc::clone(&item_repository)));
    let similar_items: Arc<dyn hermit_traits::library::SimilarItemsManager> = Arc::new(
        HermitSimilarItemsManager::new(db.clone(), Arc::clone(&item_repository)),
    );
    let search: Arc<dyn hermit_traits::library::SearchManager> =
        Arc::new(HermitSearchManager::new(Arc::clone(&item_repository)));
    // Kept concrete so the "Migrate Trickplay Image Location" task can call the
    // inherent `move_generated_trickplay_data` helper.
    let trickplay_impl = Arc::new(HermitTrickplayManager::new(
        db.clone(),
        Arc::clone(&path_manager),
        Arc::clone(&config_trait),
        Arc::clone(&item_repository),
        Arc::new(TrickplayFrameExtractorImpl::new(
            Arc::new(TokioTranscoder::new()),
            ffmpeg.ffmpeg.to_string_lossy().into_owned(),
        )),
        Arc::new(ImageCrateEncoder::new()),
    ));
    let trickplay: Arc<dyn hermit_traits::trickplay::TrickplayManager> = trickplay_impl.clone();

    // ---- library + media-sources (consume repositories/services) ----------
    // The virtual-folder manager (shared with `with_virtual_folders` below) and
    // the filesystem scanner the library manager runs on `queue_library_scan`.
    let virtual_folders: Arc<dyn hermit_traits::library::VirtualFolderManager> = Arc::new(
        hermit_core::HermitVirtualFolderManager::new(paths.default_user_views_path())
            .with_item_store(Arc::clone(&item_persistence_service)),
    );
    let mut scanner = hermit_core::LibraryScanner::new(
        Arc::clone(&virtual_folders),
        Arc::clone(&file_system),
        Arc::clone(&item_persistence_service),
    )
    // Probe each media file during the scan (duration/size + per-stream codecs)
    // so the web client can pick direct play and the transcoder has stream info.
    .with_probe(
        Arc::clone(&media_encoder),
        Arc::clone(&media_stream_repository),
        Arc::clone(&chapter_repository),
    )
    // Fetch remote artwork (TMDB) for movies/series with no local images,
    // using Jellyfin's built-in key so posters/backdrops appear with no setup.
    .with_metadata(Arc::clone(&tmdb_client), metadata_library)
    // TheTVDB is the TV authority: series/episode metadata + artwork come from
    // TVDB during the scan (TMDB stays the fallback for a series TVDB can't match).
    .with_tvdb(Arc::clone(&the_tvdb))
    // fanart.tv artwork (logos/clear-art/disc/banners on top of TMDB's
    // poster/backdrop), keyed off the Tmdb/Imdb/Tvdb ids persisted during scan.
    // Built-in key works keyless; HERMIT_FANART_KEY adds a personal client_key.
    .with_fanart(Arc::new(hermit_providers::FanartClient::new(
        (!config.fanart_personal_api_key.is_empty())
            .then(|| config.fanart_personal_api_key.clone()),
    )))
    // Rotten Tomatoes critic ratings via OMDb — enabled only when an OMDb API
    // key is configured (HERMIT_OMDB_KEY / config.toml `omdb_api_key`).
    .with_omdb(Arc::new(hermit_providers::OmdbClient::new(
        &config.omdb_api_key,
    )))
    // Persist TMDB cast/crew credits fetched alongside the metadata.
    .with_people(Arc::clone(&people_repository))
    // Compute each artwork's dimensions + blurhash during the scan (feeds the DTO's
    // Width/Height + ImageBlurHashes).
    .with_image_processor(Arc::clone(&image_processor));
    // Scan-progress log cadence (bootstrap knob); `None` keeps the 100-item default.
    if let Some(every) = config.scan_progress_every {
        scanner = scanner.with_progress_every(every as usize);
    }
    let library_scanner = Arc::new(scanner);
    // Kept concrete so the library monitor can take it as a `LibraryScanTrigger`
    // (the `dyn LibraryManager` object does not carry that narrow impl).
    let library_impl = Arc::new(
        HermitLibraryManager::new(
            Arc::clone(&item_repository),
            Arc::clone(&item_count_service),
            Arc::clone(&item_persistence_service),
            Arc::clone(&people_repository),
        )
        .with_scanner(Arc::clone(&library_scanner)),
    );
    let library: Arc<dyn hermit_traits::library::LibraryManager> = library_impl.clone();
    // The library monitor backs the external-source change webhooks
    // (`POST /Library/{Series,Movies,Media}/{Added,Updated}`): a reported path
    // queues a (coalescing) library scan so tools like Radarr/Sonarr can poke the
    // server to pick up new media. Live OS inotify watching is deferred, so the
    // watcher is a no-op — only the webhook-driven refresh path is wired.
    let library_monitor: Arc<dyn hermit_traits::library::LibraryMonitor> = Arc::new(
        hermit_core::HermitLibraryMonitor::new(
            Arc::new(hermit_core::NoopFileSystemWatcher),
            Vec::new(),
        )
        .with_refresh_target(library_impl.clone()),
    );
    // Scheduled tasks: the registry + trigger scheduler behind the dashboard's
    // "Scheduled Tasks" page. Trigger overrides persist across restarts; the
    // full Library/Maintenance task set is registered below once its backing
    // managers exist, and the scheduler starts after registration.
    let task_manager = HermitTaskManager::new();
    task_manager.set_trigger_store(config.config_dir.join("task_triggers.json"));
    task_manager.register(Arc::new(hermit_core::RefreshLibraryTask::new(Arc::clone(
        &library,
    ))));
    // The curated, compiled-in extensions (Intro Skipper, …). Their descriptors
    // feed the plugin manager below (so they appear in `/Plugins`); their tasks
    // are registered once `media_segments` exists, and the `task_manager` is
    // wrapped into the `tasks` seam after that.
    let extensions = hermit_extensions::builtin_extensions();
    let media_sources: Arc<dyn hermit_traits::library::MediaSourceManager> = Arc::new(
        HermitMediaSourceManager::new(
            Arc::clone(&item_repository),
            Arc::clone(&media_stream_repository),
            Arc::clone(&media_attachment_repository),
            Arc::clone(&media_encoder),
            Arc::clone(&providers),
        )
        .with_live_tv(Arc::clone(&live_tv)),
    );

    // ---- managers over library -------------------------------------------
    let chapters: Arc<dyn hermit_traits::chapters::ChapterManager> = Arc::new(
        HermitChapterManager::new(Arc::clone(&chapter_repository), Arc::clone(&library)),
    );
    // The Tier-1 plugin manager is built here (ahead of its `with_plugins`
    // injection) so the OpenSubtitles subtitle provider can read its
    // dashboard-managed credentials through it. The OpenSubtitles plugin is
    // registered so it appears in the dashboard and its `{ApiKey,Username,
    // Password}` config is settable via `POST /Plugins/{id}/Configuration`.
    let mut registered_plugins = vec![
        hermit_core::RegisteredPlugin::new(
            hermit_traits::plugins::PluginDescriptor {
                id: hermit_providers::opensubtitles::PLUGIN_ID,
                name: "OpenSubtitles".to_owned(),
                version: "1.0.0".to_owned(),
                description: "Download subtitles from opensubtitles.com".to_owned(),
                enabled: true,
                has_image: false,
                can_uninstall: false,
            },
            None,
        )
        .with_default_config(br#"{"ApiKey":"","Username":"","Password":""}"#.to_vec()),
    ];
    // Every curated extension surfaces as a plugin here.
    registered_plugins.extend(hermit_extensions::registered_plugins(&extensions));
    let plugins: Arc<dyn hermit_traits::plugins::PluginManager> =
        Arc::new(hermit_core::HermitPluginManager::new(
            registered_plugins,
            config.config_dir.join("plugins"),
        ));
    let subtitle_providers: Vec<Arc<dyn hermit_traits::subtitles::SubtitleProvider>> =
        vec![Arc::new(hermit_providers::OpenSubtitlesProvider::new(
            Arc::clone(&plugins),
        ))];
    let subtitles: Arc<dyn hermit_traits::subtitles::SubtitleManager> =
        Arc::new(HermitSubtitleManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&media_stream_repository),
            subtitle_providers,
            paths.internal_metadata_path(),
        ));
    let media_segments: Arc<dyn hermit_traits::media_segments::MediaSegmentManager> = Arc::new(
        HermitMediaSegmentManager::new(db.clone(), Arc::clone(&library)),
    );

    // Wire the curated extensions' background tasks now that their collaborators
    // (library, media segments, plugin config) exist. The intro skipper gets a
    // fingerprinter only when Chromaprint's `fpcalc` is installed; otherwise it
    // loads but reports unavailable at run time.
    let fingerprinter: Option<Arc<dyn hermit_extensions::fingerprint::Fingerprinter>> =
        hermit_extensions::fingerprint::discover_fpcalc().map(|fpcalc| {
            Arc::new(hermit_extensions::fingerprint::FpcalcFingerprinter::new(
                fpcalc,
                ffmpeg.ffmpeg.to_string_lossy().into_owned(),
            )) as Arc<dyn hermit_extensions::fingerprint::Fingerprinter>
        });
    // The Merge Versions extension's bulk merge/split service — shared by its
    // scheduled tasks (via the context below) and the `/MergeVersions/*` routes
    // (via `with_merge_versions` on the app state).
    let merge_versions: Arc<dyn hermit_traits::merge_versions::MergeVersionsManager> = Arc::new(
        hermit_extensions::merge_versions::MergeVersionsService::new(
            Arc::clone(&item_repository) as Arc<_>,
            Arc::clone(&item_persistence_service) as Arc<_>,
            Arc::clone(&library),
            Arc::clone(&virtual_folders),
            Arc::clone(&plugins),
        ),
    );
    let extension_cx = hermit_extensions::ExtensionContext {
        library: Arc::clone(&library),
        media_segments: Arc::clone(&media_segments),
        plugins: Arc::clone(&plugins),
        fingerprinter,
        cache_dir: config.cache_dir.join("extensions"),
        merge_versions: Arc::clone(&merge_versions),
    };
    hermit_extensions::register_tasks(&extensions, &extension_cx, &task_manager);
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

    // The full Jellyfin dashboard task set (Library + Maintenance categories),
    // now that every backing manager exists. Once registered, the scheduler
    // fires their default (or overridden) triggers.
    {
        use hermit_core::scheduled_tasks::library as lib_tasks;
        use hermit_core::scheduled_tasks::maintenance as maint_tasks;
        let paths_dyn: Arc<dyn hermit_traits::system::ServerApplicationPaths> =
            Arc::clone(&paths) as Arc<_>;
        task_manager.register(Arc::new(lib_tasks::KeyframeExtractionTask::new(
            Arc::clone(&library),
            Arc::clone(&keyframe_repository),
            Arc::clone(&media_encoder),
        )));
        task_manager.register(Arc::new(lib_tasks::AudioNormalizationTask::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&virtual_folders),
            Arc::clone(&media_encoder),
            Arc::new(lib_tasks::TokioFfmpegRunner),
            Arc::clone(&paths_dyn),
        )));
        task_manager.register(Arc::new(lib_tasks::ChapterImagesTask::new(
            Arc::clone(&library),
            Arc::clone(&virtual_folders),
            Arc::clone(&chapters),
            Arc::clone(&media_stream_repository),
            Arc::clone(&media_encoder),
            Arc::clone(&path_manager),
            Arc::clone(&paths_dyn),
        )));
        task_manager.register(Arc::new(lib_tasks::PeopleValidationTask::new(
            db.clone(),
            Arc::clone(&providers),
        )));
        task_manager.register(Arc::new(lib_tasks::SubtitleDownloadTask::new(
            Arc::clone(&library),
            Arc::clone(&virtual_folders),
            Arc::clone(&subtitles),
            Arc::clone(&media_stream_repository),
        )));
        task_manager.register(Arc::new(lib_tasks::LyricDownloadTask::new(
            Arc::clone(&library),
            Arc::clone(&lyrics),
        )));
        task_manager.register(Arc::new(lib_tasks::TrickplayImagesTask::new(
            Arc::clone(&library),
            Arc::clone(&trickplay),
        )));
        task_manager.register(Arc::new(maint_tasks::CleanActivityLogTask::new(
            Arc::clone(&config_trait),
            Arc::clone(&activity),
        )));
        task_manager.register(Arc::new(maint_tasks::DeleteCacheFileTask::new(Arc::clone(
            &paths_dyn,
        ))));
        task_manager.register(Arc::new(maint_tasks::DeleteLogFileTask::new(
            Arc::clone(&config_trait),
            server_config.log_file_retention_days,
        )));
        task_manager.register(Arc::new(
            maint_tasks::CleanupCollectionAndPlaylistPathsTask::new(
                Arc::clone(&library),
                Arc::clone(&collections),
                Arc::clone(&playlists),
                Arc::clone(&linked_children_service),
            ),
        ));
        task_manager.register(Arc::new(maint_tasks::CleanupUserDataTask::new(db.clone())));
        task_manager.register(Arc::new(maint_tasks::MoveTrickplayImagesTask::new(
            Arc::clone(&trickplay_impl),
        )));
    }
    let tasks: Arc<dyn hermit_traits::tasks::TaskManager> = Arc::new(task_manager.clone());
    // The trigger scheduler: fires startup triggers now, then evaluates
    // daily/weekly/interval triggers for the life of the process.
    drop(task_manager.start_scheduler());
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
    // The session message bus is created here (not with SyncPlay below) because
    // the session manager needs it too: a bus-registered `/socket` sink is what
    // makes a session remote-controllable (cast-to-device), and it is the
    // delivery path for Play/Playstate/GeneralCommand pushes.
    let session_bus: Arc<dyn hermit_traits::session_bus::SessionMessageBus> =
        Arc::new(hermit_core::HermitSessionMessageBus::new());
    let sessions: Arc<dyn hermit_traits::session::SessionManager> = Arc::new(
        HermitSessionManager::new(
            Arc::clone(&users),
            Arc::clone(&devices),
            Arc::clone(&user_data),
            Arc::clone(&library),
            Arc::clone(&dto),
            Arc::clone(&event_manager),
            db.clone(),
            server_id.clone(),
        )
        .with_session_bus(Arc::clone(&session_bus)),
    );

    // These two maintenance tasks gate on active playback, so they register
    // once the session manager exists (registration order is otherwise inert).
    {
        use hermit_core::scheduled_tasks::maintenance as maint_tasks;
        let paths_dyn: Arc<dyn hermit_traits::system::ServerApplicationPaths> =
            Arc::clone(&paths) as Arc<_>;
        task_manager.register(Arc::new(maint_tasks::DeleteTranscodeFileTask::new(
            paths_dyn,
            Arc::clone(&sessions),
        )));
        task_manager.register(Arc::new(maint_tasks::OptimizeDatabaseTask::new(
            db.clone(),
            Arc::clone(&sessions),
        )));
    }
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
    let system: Arc<dyn hermit_traits::system::SystemManager> = Arc::new(
        HermitSystemManager::new(
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
        )
        .with_library_storage(Arc::new(VirtualFolderStorage(Arc::clone(&virtual_folders)))),
    );

    // The auth service wraps an owned concrete authorization context, so build
    // that concrete value, clone it into the service, and box the other for the
    // `auth_context` slot.
    let auth_context_concrete = HermitAuthorizationContext::new(
        db.clone(),
        Arc::clone(&users),
        Arc::clone(&app_host_trait),
        Arc::clone(&config_trait),
        server_id.clone(),
        crate::service_version(),
    )
    .with_auth_cache(Arc::clone(&auth_cache));
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
    let (hls, attachments, subtitle_encoder) = build_media_encoding(
        me_media_sources,
        me_media_encoder,
        me_config,
        Arc::clone(&paths),
        me_path_manager,
        ffmpeg,
    );
    let state = state
        .with_media_encoding(hls, attachments)
        .with_subtitle_encoder(subtitle_encoder);

    // ---- virtual-folder (library-structure) store -------------------------
    // The `/Library/VirtualFolders*` + `/Library/PhysicalPaths` admin surface is
    // filesystem-backed: each library is a directory under `DefaultUserViewsPath`
    // (`.mblink` shortcuts + `options.json`). Replace the disabled stub
    // `AppState::new` installed with the real filesystem-backed manager rooted
    // there (mirrors the C# `ILibraryManager` virtual-folder methods).
    let state = state.with_virtual_folders(Arc::clone(&virtual_folders));

    // Image serving runs through the real `image`-crate processor so
    // `maxWidth`/`format`/… requests resize/convert instead of serving the original.
    let state = state.with_image_processor(Arc::clone(&image_processor));

    // Replace the `NoopLibraryMonitor` default with the webhook-driven monitor
    // built above, so `POST /Library/*/{Added,Updated}` actually refreshes.
    let state = state.with_library_monitor(library_monitor);

    // ---- plugin manager (Tier 1: compile-time plugins) --------------------
    // Backs `/Plugins/*`, `/Packages/*`, and `/Repositories` over the compile-time
    // plugin registry (built above with the OpenSubtitles plugin registered). The
    // manager persists the repository list and per-plugin configuration under
    // `{config}/plugins/`. Runtime install/load is Tier 2 (a WASM/libloading
    // host). See brain/PLAN_HERMIT_PLUGINS.md.
    let state = state.with_plugins(Arc::clone(&plugins));

    // ---- SyncPlay ---------------------------------------------------------
    // The SyncPlay manager shares the session message bus (created with the
    // session manager above) to deliver group commands to member sockets.
    let sync_play: Arc<dyn hermit_traits::stubs::SyncPlayManager> = Arc::new(
        hermit_core::HermitSyncPlayManager::new(Arc::clone(&session_bus)),
    );
    // ---- File Transformation pipeline --------------------------------------
    // The registry the static `/web` mount consults per request. The Intro
    // Skipper's skip-button patch for `main.jellyfin.bundle.js` is its
    // compiled-in registration (the upstream plugin registers it via .NET
    // reflection); both transformers self-gate on their plugin's enabled flag
    // and configuration, so dashboard toggles apply live.
    let file_transformations: Arc<dyn hermit_traits::plugins::FileTransformationService> = Arc::new(
        hermit_extensions::file_transformation::WebFileTransformationService::new(
            Arc::clone(&plugins),
            format!("http://127.0.0.1:{}", config.port),
        ),
    );
    hermit_extensions::file_transformation::register_skip_button_transformer(
        file_transformations.as_ref(),
        Arc::clone(&plugins),
    )
    .await;

    // ---- playback metrics (brain/PLAN_PERFORMANCE.md Track A) --------------
    let playback_metrics: Arc<dyn hermit_traits::metrics::PlaybackMetrics> =
        Arc::new(hermit_core::HermitPlaybackMetrics::new(db.clone()));

    let state = state
        .with_session_bus(Arc::clone(&session_bus))
        .with_sync_play(sync_play)
        .with_live_tv(live_tv)
        .with_file_transformations(Arc::clone(&file_transformations))
        .with_merge_versions(merge_versions)
        .with_playback_metrics(playback_metrics);

    Ok(WiredApp {
        state,
        app_host,
        file_transformations,
    })
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
            omdb_api_key: String::new(),
            studios_repo_url: String::new(),
            tvdb_api_key: String::new(),
            tvdb_subscriber_pin: String::new(),
            fanart_personal_api_key: String::new(),
            ffmpeg_path: None,
            ffprobe_path: None,
            library_roots: Vec::new(),
            server_name: "hermit-test".to_owned(),
            log_level: "info".to_owned(),
            admin_user: "admin".to_owned(),
            admin_password: String::new(),
            db_pool: None,
            enable_metrics: None,
            metrics_sample_interval: None,
            scan_progress_every: None,
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
            filters: Vec::new(),
            encoders: Vec::new(),
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
