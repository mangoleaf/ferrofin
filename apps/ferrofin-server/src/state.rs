//! Composition root — assembles every concrete `ferrofin-core` manager into a
//! `ferrofin_api::AppState`.
//!
//! Port of the Autofac service-registration in `Jellyfin.Server`'s `Startup`
//! plus `CoreAppHost.RegisterServices`: it constructs the managers in strict
//! dependency order (leaf repositories first, then the services that consume
//! them, then the managers that consume those services) and injects them as the
//! `Arc<dyn Trait>` fields of [`AppState`].
//!
//! The construction order is the topological order of the manager dependency
//! DAG. This unit wires the 33 core managers
//! and then replaces the media-encoding seams (`hls` / `attachments`) — installed
//! as disabled stubs by [`AppState::new`] — with the real ffmpeg-backed transcode
//! pair via [`with_media_encoding`](AppState::with_media_encoding) (built by
//! [`build_media_encoding`](crate::media_encoding::build_media_encoding)).

use std::sync::Arc;

use anyhow::Context as _;
use ferrofin_api::AppState;
use ferrofin_core::application_host::HostNetworkInfo;
use ferrofin_core::system_manager::{LifecycleController, SystemHostFacts};
use ferrofin_core::{
    FerrofinActivityManager, FerrofinApiKeyManager, FerrofinAuthService,
    FerrofinAuthorizationContext, FerrofinChapterManager, FerrofinChapterRepository,
    FerrofinClientEventLogger, FerrofinCollectionManager, FerrofinDeviceManager,
    FerrofinDisplayPreferencesManager, FerrofinDtoService, FerrofinEventManager,
    FerrofinExternalDataManager, FerrofinFileSystem, FerrofinItemCountService,
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinKeyframeRepository,
    FerrofinLibraryManager, FerrofinLyricManager, FerrofinMediaAttachmentRepository,
    FerrofinMediaSegmentManager, FerrofinMediaSourceManager, FerrofinMediaStreamRepository,
    FerrofinMusicManager, FerrofinNextUpService, FerrofinPathManager, FerrofinPeopleRepository,
    FerrofinPlaylistManager, FerrofinQuickConnect, FerrofinSearchManager,
    FerrofinServerApplicationHost, FerrofinServerConfigurationManager, FerrofinSessionManager,
    FerrofinSimilarItemsManager, FerrofinSubtitleManager, FerrofinSystemManager,
    FerrofinTaskManager, FerrofinTrickplayManager, FerrofinTvSeriesManager,
    FerrofinUserDataManager, FerrofinUserManager, FerrofinUserViewManager,
    HermitLinkedChildrenService, ItemTypeLookup, LocalizationManager,
};
use ferrofin_db::Database;
use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
use ferrofin_livetv::FerrofinLiveTvManager;
use ferrofin_mediaencoding::{
    MediaEncoderConfig, MediaEncoderImpl, TokioTranscoder, TrickplayFrameExtractorImpl,
};
use ferrofin_providers::LocalProviderManager;
use ferrofin_traits::system::ServerApplicationPaths as _;

use crate::bootstrap::FfmpegPaths;
use crate::config::Config;
use crate::media_encoding::build_media_encoding;

/// The plugin-analysis decode budget: a quarter of the visible cores.
fn num_cpus_for_analysis() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get() / 4)
}

/// The product / package identity advertised by the server, ported from
/// Jellyfin's `ApplicationHost` constants.
const PRODUCT_NAME: &str = "Jellyfin Server";

/// The package name reported in system info (`IStartupOptions.PackageName`).
const PACKAGE_NAME: &str = "ferrofin-server";

/// The **Jellyfin server version Ferrofin reports** in `SystemInfo`/`PublicSystemInfo`
/// (`Version`). This is the version the vendored OpenAPI contract targets — i.e.
/// "Ferrofin speaks Jellyfin 10.11.8's API" — NOT Ferrofin's own crate version
/// (`CARGO_PKG_VERSION`, used only for build/log lines). Clients gate on it:
/// jellyfin-web's SDK refuses any server below `MINIMUM_VERSION = 10.10.0` with an
/// "Update Required" screen, so reporting Ferrofin's `0.1.0` locks the web client out.
/// Keep this in sync with `contracts/jellyfin-openapi-*.json`.
const JELLYFIN_API_VERSION: &str = "10.11.8";

/// The assembled application state plus the handles the composition root still
/// needs after wiring (the concrete host, to flip its startup flag and drive
/// name refresh, and the lifecycle controller's restart flag).
pub struct WiredApp {
    /// The fully-wired shared state handed to every axum handler.
    pub state: AppState,
    /// The concrete host — the composition root calls
    /// [`FerrofinServerApplicationHost::mark_core_startup_complete`] on it once
    /// the router is mounted, mirroring `CoreAppHost`'s post-startup flag.
    pub app_host: Arc<FerrofinServerApplicationHost>,
    /// The web-file transformation pipeline, shared with the static `/web`
    /// mount so registered transformations apply to the served files.
    pub file_transformations: Arc<dyn ferrofin_traits::plugins::FileTransformationService>,
}

/// The concrete [`LifecycleController`] for the running server.
///
/// Port of the slice of `IHostApplicationLifetime` the system manager drives:
/// `stop(restart)` records whether a restart was requested and signals the axum
/// graceful-shutdown handle; the `has_pending_restart` / `is_shutting_down`
/// flags mirror `IServerApplicationHost.HasPendingRestart` / the host's
/// shutdown state. Only a test `Fake` existed before this unit.
pub struct FerrofinLifecycleController {
    /// Fires the axum `with_graceful_shutdown` future; taken on the first stop.
    shutdown: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Set once a stop has been requested (the server is winding down).
    shutting_down: std::sync::atomic::AtomicBool,
    /// Set when the requested stop should be followed by a restart.
    restart_pending: std::sync::atomic::AtomicBool,
}

impl FerrofinLifecycleController {
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
impl LifecycleController for FerrofinLifecycleController {
    async fn stop(&self, restart: bool) -> Result<(), ferrofin_traits::error::ServiceError> {
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

    fn mark_restart_required(&self) {
        self.restart_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Adapts the [`VirtualFolderManager`](ferrofin_traits::library::VirtualFolderManager)
/// to the system manager's [`LibraryStorageProvider`] seam, so the storage page
/// reports each library folder's real disk usage.
struct VirtualFolderStorage(Arc<dyn ferrofin_traits::library::VirtualFolderManager>);

#[async_trait::async_trait]
impl ferrofin_core::system_manager::LibraryStorageProvider for VirtualFolderStorage {
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
/// `shutdown` sender is handed to the [`FerrofinLifecycleController`] so a
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
    let paths = Arc::new(ferrofin_core::FerrofinServerApplicationPaths::new(
        &config.data_dir,
        config.data_dir.join("log"),
        &config.config_dir,
        &config.cache_dir,
        &config.web_dir,
    ));

    // ---- configuration manager (loads persisted system.json) --------------
    let config_mgr = Arc::new(
        FerrofinServerConfigurationManager::load(Arc::clone(&paths))
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
    let item_type_lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    let item_repository: Arc<dyn ferrofin_traits::persistence::ItemRepository> = Arc::new(
        FerrofinItemRepository::new(db.clone(), Arc::clone(&item_type_lookup)),
    );
    let item_count_service: Arc<dyn ferrofin_traits::persistence::ItemCountService> =
        Arc::new(FerrofinItemCountService::new(db.clone()));
    let item_persistence_service: Arc<dyn ferrofin_traits::persistence::ItemPersistenceService> =
        Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    // The per-database item-id derivation mode: Jellyfin 10.11.8 parity
    // (case-sensitive + data-dir-relative rewrite) for fresh and adopted
    // databases, grandfathered lowercase for pre-parity Ferrofin ones
    // (`HermitMeta.item_id_derivation`, seeded by migration 0009). Resolved
    // here because the people repository needs it for per-name person ids.
    let id_derivation = ferrofin_core::item_type_lookup::IdDerivation::from_meta(
        db.meta_get("item_id_derivation")
            .await
            .context("failed to read the item-id derivation mode")?
            .as_deref(),
        Some(paths.program_data_path()),
    );
    let people_repository_impl = Arc::new(
        FerrofinPeopleRepository::new(db.clone())
            .with_identity(id_derivation.clone(), paths.people_path()),
    );
    // One-shot: collapse pre-unification per-(name, type) person items onto
    // the deterministic per-name id (repointing favorites/images), so a
    // favorited person reads back from every credit surface.
    match people_repository_impl.unify_person_identities().await {
        Ok(0) => {}
        Ok(collapsed) => {
            tracing::info!(collapsed, "unified person identities onto per-name ids");
        }
        Err(err) => {
            tracing::warn!(%err, "person identity unification failed; person favorites may not round-trip");
        }
    }
    let people_repository: Arc<dyn ferrofin_traits::persistence::PeopleRepository> =
        people_repository_impl;
    let media_stream_repository: Arc<dyn ferrofin_traits::persistence::MediaStreamRepository> =
        Arc::new(FerrofinMediaStreamRepository::new(db.clone()));
    let media_attachment_repository: Arc<
        dyn ferrofin_traits::persistence::MediaAttachmentRepository,
    > = Arc::new(FerrofinMediaAttachmentRepository::new(db.clone()));
    let chapter_repository: Arc<dyn ferrofin_traits::persistence::ChapterRepository> =
        Arc::new(FerrofinChapterRepository::new(db.clone()));
    let keyframe_repository: Arc<dyn ferrofin_traits::persistence::KeyframeRepository> =
        Arc::new(FerrofinKeyframeRepository::new(db.clone()));
    let linked_children_service: Arc<dyn ferrofin_traits::persistence::LinkedChildrenService> =
        Arc::new(HermitLinkedChildrenService::new(db.clone()));
    let next_up_service: Arc<dyn ferrofin_traits::persistence::NextUpService> =
        Arc::new(FerrofinNextUpService::new(db.clone()));

    // ---- standalone leaves (no manager deps) ------------------------------
    // The real `image`-crate encoder (resize + format-convert), not the no-op
    // `NullImageEncoder` — so `maxWidth`/`fillHeight`/`format` image requests are
    // honoured instead of serving the full-size original.
    let image_processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> = Arc::new(
        ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), paths.image_cache_path()),
    );
    // The shared TMDB client — the scan's automatic artwork, the remote-search
    // ("Identify") providers, and the remote-image ("Choose Image") methods all
    // use Jellyfin's built-in key.
    let tmdb_client = Arc::new(ferrofin_providers::TmdbClient::new());
    let metadata_library = std::path::PathBuf::from(paths.internal_metadata_path()).join("library");
    // TheTVDB — the TV authority. Ships on with the built-in project key (like
    // TMDB); a user key/PIN override enables their subscription tier.
    let the_tvdb = Arc::new(ferrofin_providers::TvdbClient::with_config(
        &config.tvdb_api_key,
        &config.tvdb_subscriber_pin,
    ));
    let search_providers: Vec<Arc<dyn ferrofin_providers::RemoteSearchProvider>> = vec![
        Arc::new(ferrofin_providers::TmdbSearchProvider::new(
            Arc::clone(&tmdb_client),
            ferrofin_providers::TmdbKind::Movie,
        )),
        Arc::new(ferrofin_providers::TmdbSearchProvider::new(
            Arc::clone(&tmdb_client),
            ferrofin_providers::TmdbKind::Series,
        )),
        Arc::new(ferrofin_providers::TvdbSearchProvider::new(Arc::clone(
            &the_tvdb,
        ))),
    ];
    // Studio logos from the artwork repository (name-matched, keyless). The repo
    // URL is overridable; empty falls back to the built-in emby-artwork tree.
    let studios_client = Arc::new(ferrofin_providers::StudiosClient::with_repo_url(
        &config.studios_repo_url,
    ));
    let providers: Arc<dyn ferrofin_traits::providers::ProviderManager> = Arc::new(
        LocalProviderManager::new(Vec::new())
            .with_image_store(
                Arc::clone(&item_persistence_service),
                metadata_library.clone(),
            )
            .with_remote_images(Arc::clone(&tmdb_client), Arc::clone(&item_repository))
            .with_remote_search_providers(search_providers)
            .with_studios(Arc::clone(&studios_client)),
    );
    let file_system: Arc<dyn ferrofin_traits::filesystem::FileSystem> =
        Arc::new(FerrofinFileSystem::new());
    // Kept concrete alongside the trait handle: consumers subscribe on the
    // concrete bus (below, once the session manager exists) while publishers
    // take the `dyn EventManager` seam. Clones share one registry.
    let event_bus = FerrofinEventManager::new();
    let event_manager: Arc<dyn ferrofin_traits::events::EventManager> = Arc::new(event_bus.clone());
    let localization: Arc<dyn ferrofin_traits::localization::LocalizationManager> = Arc::new(
        LocalizationManager::new(&server_config.metadata_country_code),
    );
    let lyric_providers: Vec<Arc<dyn ferrofin_traits::stubs::LyricProvider>> =
        vec![Arc::new(ferrofin_providers::LrcLibProvider::new())];
    let lyrics: Arc<dyn ferrofin_traits::stubs::LyricManager> = Arc::new(
        FerrofinLyricManager::new()
            .with_items(Arc::clone(&item_repository))
            .with_providers(lyric_providers),
    );
    let live_tv: Arc<dyn ferrofin_traits::stubs::LiveTvManager> =
        Arc::new(FerrofinLiveTvManager::new(
            db.clone(),
            Arc::new(ferrofin_livetv::ReqwestFetcher::new()),
            server_id.clone(),
        ));
    let path_manager: Arc<dyn ferrofin_traits::system::PathManager> =
        Arc::new(FerrofinPathManager::new(Arc::clone(&paths)));
    let client_event_logger: Arc<dyn ferrofin_traits::events::ClientEventLogger> =
        Arc::new(FerrofinClientEventLogger::new(
            Arc::clone(&paths) as Arc<dyn ferrofin_traits::system::ServerApplicationPaths>
        ));

    // ---- media encoder (probe-only; ffmpeg process seam) ------------------
    let media_encoder: Arc<dyn ferrofin_traits::media_encoding::MediaEncoder> =
        Arc::new(MediaEncoderImpl::new(
            Arc::new(TokioTranscoder::new()),
            ffmpeg.ffmpeg.to_string_lossy().into_owned(),
            ffmpeg.ffprobe.to_string_lossy().into_owned(),
            MediaEncoderConfig {
                analyze_duration: None,
                probe_size: None,
                threads: 0,
                // Frame extraction (chapter images) writes here, never next to
                // the media file — media mounts are commonly read-only. The
                // C# `TempDirectory` layout: a `temp` dir under the cache.
                temp_dir: std::path::PathBuf::from(paths.cache_path()).join("temp"),
            },
        ));

    // ---- config trait object (shared by many managers) --------------------
    let config_trait: Arc<dyn ferrofin_traits::configuration::ServerConfigurationManager> =
        Arc::clone(&config_mgr) as Arc<_>;

    // ---- managers over repositories/services ------------------------------
    // ONE auth cache shared by the authorization context (read-through) and the
    // user/device managers (invalidation on mutation) — the sharing is what
    // makes cached auth revocation-correct. See ferrofin_core::auth_cache.
    let auth_cache = Arc::new(ferrofin_core::auth_cache::AuthCache::default());
    let activity: Arc<dyn ferrofin_traits::activity::ActivityManager> =
        Arc::new(FerrofinActivityManager::new(db.clone()));
    let users: Arc<dyn ferrofin_traits::library::UserManager> = Arc::new(
        FerrofinUserManager::new(db.clone())
            .with_server_id(server_id.clone())
            .with_profile_image_dir(
                std::path::PathBuf::from(paths.internal_metadata_path()).join("users"),
            )
            .with_auth_cache(Arc::clone(&auth_cache))
            // A lockout is a dashboard Alert, not just a log line.
            .with_activity(Arc::clone(&activity))
            // Resolves each user's CastReceiverId against the configured
            // receivers — jellyfin-web shows no cast devices without one.
            .with_configuration(Arc::clone(&config_trait)),
    );
    let user_data: Arc<dyn ferrofin_traits::library::UserDataManager> = Arc::new(
        FerrofinUserDataManager::new(db.clone(), Arc::clone(&config_trait)),
    );
    let devices: Arc<dyn ferrofin_traits::devices::DeviceManager> =
        Arc::new(FerrofinDeviceManager::new(db.clone()).with_auth_cache(Arc::clone(&auth_cache)));
    let api_keys: Arc<dyn ferrofin_traits::security::ApiKeyManager> =
        Arc::new(FerrofinApiKeyManager::new(db.clone()));
    let display_preferences: Arc<dyn ferrofin_traits::configuration::DisplayPreferencesManager> =
        Arc::new(FerrofinDisplayPreferencesManager::new(db.clone()));
    // The playlists media folder lives at `{data}/playlists` (C#
    // `ManualPlaylistsFolder`); the user-view seam provisions it lazily.
    let playlists_path = std::path::PathBuf::from(paths.data_path()).join("playlists");
    let user_views: Arc<dyn ferrofin_traits::library::UserViewManager> = Arc::new(
        FerrofinUserViewManager::new(Arc::clone(&item_repository))
            .with_playlists_store(Arc::clone(&item_persistence_service), playlists_path)
            .with_id_derivation(id_derivation.clone()),
    );
    let music: Arc<dyn ferrofin_traits::library::MusicManager> =
        Arc::new(FerrofinMusicManager::new(Arc::clone(&item_repository)));
    let similar_items: Arc<dyn ferrofin_traits::library::SimilarItemsManager> = Arc::new(
        FerrofinSimilarItemsManager::new(db.clone(), Arc::clone(&item_repository)),
    );
    let search: Arc<dyn ferrofin_traits::library::SearchManager> =
        Arc::new(FerrofinSearchManager::new(Arc::clone(&item_repository)));
    // Kept concrete so the "Migrate Trickplay Image Location" task can call the
    // inherent `move_generated_trickplay_data` helper.
    let trickplay_impl = Arc::new(FerrofinTrickplayManager::new(
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
    let trickplay: Arc<dyn ferrofin_traits::trickplay::TrickplayManager> = trickplay_impl.clone();

    // ---- library + media-sources (consume repositories/services) ----------
    // The virtual-folder manager (shared with `with_virtual_folders` below) and
    // the filesystem scanner the library manager runs on `queue_library_scan`.
    // Kept concrete so the library monitor can take it as its `WatchRootsSource`
    // (the roots of every library with realtime monitoring enabled).
    let virtual_folders_impl = Arc::new(
        ferrofin_core::FerrofinVirtualFolderManager::new(paths.default_user_views_path())
            .with_item_store(Arc::clone(&item_persistence_service))
            .with_id_derivation(id_derivation.clone()),
    );
    let virtual_folders: Arc<dyn ferrofin_traits::library::VirtualFolderManager> =
        virtual_folders_impl.clone();
    let mut scanner = ferrofin_core::LibraryScanner::new(
        Arc::clone(&virtual_folders),
        Arc::clone(&file_system),
        Arc::clone(&item_persistence_service),
    )
    .with_id_derivation(id_derivation)
    // OfficialRating → numeric parental score on each scanned row (the
    // Parental Rating sort and max-rating filters read the numeric column).
    .with_localization(Arc::new(LocalizationManager::new(
        &server_config.metadata_country_code,
    )))
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
    // Built-in key works keyless; FERROFIN_FANART_KEY adds a personal client_key.
    .with_fanart(Arc::new(ferrofin_providers::FanartClient::new(
        (!config.fanart_personal_api_key.is_empty())
            .then(|| config.fanart_personal_api_key.clone()),
    )))
    // Rotten Tomatoes critic ratings via OMDb — enabled only when an OMDb API
    // key is configured (FERROFIN_OMDB_KEY / config.toml `omdb_api_key`).
    .with_omdb(Arc::new(ferrofin_providers::OmdbClient::new(
        &config.omdb_api_key,
    )))
    // Persist TMDB cast/crew credits fetched alongside the metadata.
    .with_people(Arc::clone(&people_repository))
    // Resolve MusicBrainz ids for music items in the post-scan enrichment pass
    // (the item repository lets it query the MusicAlbum/MusicArtist rows + tracks
    // it created). Keyless; a mirror URL lifts the 1 req/sec limit.
    .with_music(
        Arc::new(ferrofin_providers::MusicBrainzClient::new(
            &config.musicbrainz_base_url,
            env!("CARGO_PKG_VERSION"),
        )),
        Arc::clone(&item_repository),
    )
    // AudioDb artist bio/genre + artist/album artwork (by MusicBrainz id),
    // fetched in the post-scan music pass. Built-in free key.
    .with_audiodb(Arc::new(ferrofin_providers::AudioDbClient::new()))
    // Studio thumbs from the artwork repository, downloaded post-scan for the
    // by-name Studio rows so the TV Networks / Studios tabs carry artwork.
    .with_studio_images(Arc::clone(&studios_client))
    // Compute each artwork's dimensions + blurhash during the scan (feeds the DTO's
    // Width/Height + ImageBlurHashes).
    .with_image_processor(Arc::clone(&image_processor));
    // Scan-progress log cadence (bootstrap knob); `None` keeps the 100-item default.
    if let Some(every) = config.scan_progress_every {
        scanner = scanner.with_progress_every(every as usize);
    }
    // Scans publish `LibraryChanged` + `RefreshProgress` events; the consumers
    // registered below (once the session manager exists) forward them to
    // clients over the WebSocket so open views refresh after a scan.
    // Tier-1b: runtime-installed WASM plugins from `{data_dir}/plugins/*.wasm`
    // (see brain/plans/PLAN_PLUGIN_TIERS.md). Loading compiles components —
    // CPU-heavy, so it runs on the blocking pool. A load failure degrades to
    // "no WASM plugins", never a failed boot; per-file failures are logged
    // and skipped inside the loader.
    let wasm_host = {
        let wasm_settings = ferrofin_wasm::WasmSettings::resolve(
            config.wasm_call_timeout_secs,
            config.wasm_memory_limit_mb,
            config.wasm_event_queue_capacity,
            config.wasm_private_http_allow.as_deref(),
        )
        .with_state_limit_mb(config.wasm_state_limit_mb);
        let wasm_dir = config.data_dir.join("plugins");
        match tokio::task::spawn_blocking(move || {
            ferrofin_wasm::WasmPluginHost::load(&wasm_dir, &wasm_settings)
        })
        .await
        {
            Ok(Ok(host)) => host,
            Ok(Err(err)) => {
                tracing::warn!(%err, "wasm plugin host unavailable; continuing without it");
                ferrofin_wasm::WasmPluginHost::empty()
            }
            Err(join_err) => {
                tracing::warn!(%join_err, "wasm plugin load task panicked; continuing without it");
                ferrofin_wasm::WasmPluginHost::empty()
            }
        }
    };
    // The scan's dynamic metadata pass: every loaded WASM plugin is offered
    // each item after the built-in provider chain (supplement-only; inert
    // until the collaborators are armed below and while a plugin is
    // disabled).
    scanner = scanner.with_dynamic_providers(wasm_host.metadata_providers());
    scanner = scanner.with_events(Arc::clone(&event_manager));
    let library_scanner = Arc::new(scanner);
    // Kept concrete so the library monitor can take it as a `LibraryScanTrigger`
    // (the `dyn LibraryManager` object does not carry that narrow impl).
    let library_impl = Arc::new(
        FerrofinLibraryManager::new(
            Arc::clone(&item_repository),
            Arc::clone(&item_count_service),
            Arc::clone(&item_persistence_service),
            Arc::clone(&people_repository),
        )
        .with_scanner(Arc::clone(&library_scanner))
        // Chapter thumbnails are served from the chapter rows, not the item's
        // image rows.
        .with_chapters(Arc::clone(&chapter_repository)),
    );
    let library: Arc<dyn ferrofin_traits::library::LibraryManager> = library_impl.clone();
    // The library monitor drives refreshes from two change sources: the
    // external-source webhooks (`POST /Library/{Series,Movies,Media}/{Added,
    // Updated}` — Radarr/Sonarr pokes) and the live OS filesystem watcher over
    // the roots of every library with `EnableRealtimeMonitor` on. Watcher
    // events are pumped into `report_file_system_changed`, which suppresses
    // server-initiated writes and queues a (coalescing) scan for the rest. If
    // the OS watcher cannot initialize (inotify limits), fall back to the
    // no-op watcher — webhook-driven refresh still works.
    let (fs_watcher, fs_events): (
        Arc<dyn ferrofin_core::resolvers::FileSystemWatcher>,
        Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    ) = match ferrofin_core::NotifyFileSystemWatcher::new() {
        Ok((watcher, rx)) => (Arc::new(watcher), Some(rx)),
        Err(err) => {
            tracing::warn!(%err, "filesystem watcher unavailable; realtime library monitoring disabled");
            (Arc::new(ferrofin_core::NoopFileSystemWatcher), None)
        }
    };
    // Change reports debounce for `LibraryMonitorDelay` seconds (read live from
    // the server configuration) so a burst — a torrent finishing, a Sonarr
    // import batch — settles into one scan.
    let library_monitor: Arc<dyn ferrofin_traits::library::LibraryMonitor> = Arc::new(
        ferrofin_core::FerrofinLibraryMonitor::new(fs_watcher, virtual_folders_impl.clone())
            .with_refresh_target(library_impl.clone())
            .with_config(Arc::clone(&config_trait)),
    );
    if let Some(mut rx) = fs_events {
        let monitor = Arc::clone(&library_monitor);
        tokio::spawn(async move {
            while let Some(path) = rx.recv().await {
                if let Err(err) = monitor.report_file_system_changed(&path).await {
                    tracing::warn!(%err, path, "failed to report filesystem change");
                }
            }
        });
    }
    if let Err(err) = library_monitor.start().await {
        tracing::warn!(%err, "failed to start library monitor");
    }
    // Scheduled tasks: the registry + trigger scheduler behind the dashboard's
    // "Scheduled Tasks" page. Trigger overrides persist across restarts; the
    // full Library/Maintenance task set is registered below once its backing
    // managers exist, and the scheduler starts after registration.
    let task_manager = FerrofinTaskManager::new();
    task_manager.set_trigger_store(config.config_dir.join("task_triggers.json"));
    // Run outcomes publish `TaskCompleted` → forwarded to admin sessions as
    // the `ScheduledTaskEnded` push the dashboard's task page listens for.
    task_manager.set_event_manager(Arc::clone(&event_manager));
    // Failed runs also land in the dashboard's activity feed as `TaskFailed`
    // Alerts (port of upstream's `TaskCompletedLogger`).
    task_manager.set_activity_manager(Arc::clone(&activity));
    task_manager.register(Arc::new(ferrofin_core::RefreshLibraryTask::new(
        Arc::clone(&library),
    )));
    // The curated, compiled-in extensions (Intro Skipper, …). Their descriptors
    // feed the plugin manager below (so they appear in `/Plugins`); their tasks
    // are registered once `media_segments` exists, and the `task_manager` is
    // wrapped into the `tasks` seam after that.
    let extensions = ferrofin_extensions::builtin_extensions();
    let media_sources: Arc<dyn ferrofin_traits::library::MediaSourceManager> = Arc::new(
        FerrofinMediaSourceManager::new(
            Arc::clone(&item_repository),
            Arc::clone(&media_stream_repository),
            Arc::clone(&media_attachment_repository),
            Arc::clone(&media_encoder),
            Arc::clone(&providers),
        )
        .with_live_tv(Arc::clone(&live_tv)),
    );

    // ---- managers over library -------------------------------------------
    let chapters: Arc<dyn ferrofin_traits::chapters::ChapterManager> = Arc::new(
        FerrofinChapterManager::new(Arc::clone(&chapter_repository), Arc::clone(&library)),
    );
    // The Tier-1 plugin manager is built here (ahead of its `with_plugins`
    // injection) so the OpenSubtitles subtitle provider can read its
    // dashboard-managed credentials through it. The OpenSubtitles plugin is
    // registered so it appears in the dashboard and its `{ApiKey,Username,
    // Password}` config is settable via `POST /Plugins/{id}/Configuration`.
    let mut registered_plugins = vec![
        ferrofin_core::RegisteredPlugin::new(
            ferrofin_traits::plugins::PluginDescriptor {
                id: ferrofin_providers::opensubtitles::PLUGIN_ID,
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
    registered_plugins.extend(ferrofin_extensions::registered_plugins(&extensions));
    // Loaded WASM plugins surface on `/Plugins` exactly like compiled-in ones.
    // Guid + page-name collision rules live in one testable place —
    // `merge_plugin_registrations` (ferrofin-core) — covering both the
    // repository-install and hand-dropped-file doors.
    ferrofin_core::merge_plugin_registrations(
        &mut registered_plugins,
        wasm_host.registered_plugins(),
    );
    // The lifecycle controller is built early so the plugin manager can flag
    // restart-required after a repository install/uninstall; the system
    // manager receives the same handle further down.
    let lifecycle: Arc<dyn ferrofin_core::system_manager::LifecycleController> =
        Arc::new(FerrofinLifecycleController::new(shutdown));
    // The install-time artifact validator (component + descriptor checks) —
    // built from the same settings as the host so limits match.
    let wasm_validator: Arc<dyn ferrofin_traits::plugins::PluginArtifactValidator> = Arc::new(
        ferrofin_wasm::WasmArtifactValidator::new(
            &ferrofin_wasm::WasmSettings::resolve(
                config.wasm_call_timeout_secs,
                config.wasm_memory_limit_mb,
                config.wasm_event_queue_capacity,
                config.wasm_private_http_allow.as_deref(),
            )
            .with_state_limit_mb(config.wasm_state_limit_mb),
        )
        .map_err(|e| anyhow::anyhow!("wasm artifact validator init: {e}"))?,
    );
    let plugins: Arc<dyn ferrofin_traits::plugins::PluginManager> = Arc::new(
        ferrofin_core::FerrofinPluginManager::new(
            registered_plugins,
            config.config_dir.join("plugins"),
        )
        .with_installer(
            config.data_dir.join("plugins"),
            Arc::clone(&wasm_validator),
            Arc::clone(&lifecycle),
        )
        .with_download_cap_mb(config.max_plugin_download_mb),
    );
    let subtitle_providers: Vec<Arc<dyn ferrofin_traits::subtitles::SubtitleProvider>> =
        vec![Arc::new(ferrofin_providers::OpenSubtitlesProvider::new(
            Arc::clone(&plugins),
        ))];
    let subtitles: Arc<dyn ferrofin_traits::subtitles::SubtitleManager> =
        Arc::new(FerrofinSubtitleManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&media_stream_repository),
            subtitle_providers,
            paths.internal_metadata_path(),
        ));
    let media_segments: Arc<dyn ferrofin_traits::media_segments::MediaSegmentManager> = Arc::new(
        FerrofinMediaSegmentManager::new(db.clone(), Arc::clone(&library)),
    );

    // Wire the curated extensions' background tasks now that their collaborators
    // (library, media segments, plugin config) exist. The intro skipper gets a
    // fingerprinter only when a Chromaprint backend exists — ffmpeg's
    // `chromaprint` muxer, else `fpcalc`; otherwise it loads but reports
    // unavailable at run time.
    let fingerprinter: Option<Arc<dyn ferrofin_extensions::fingerprint::Fingerprinter>> =
        ferrofin_extensions::fingerprint::ChromaprintFingerprinter::discover(
            &ffmpeg.ffmpeg.to_string_lossy(),
        )
        .map(|fp| {
            tracing::debug!(backend = fp.backend(), "intro skipper: fingerprint backend");
            Arc::new(
                // The `fpcalc` fallback decodes the credits window under the
                // server's cache dir, not the system temp dir: a container's
                // /tmp is routinely small or read-only, and the failed decode
                // silently cost every "Skip Credits" segment.
                fp.with_scratch_dir(
                    std::path::PathBuf::from(paths.cache_path()).join("extensions"),
                ),
            ) as Arc<dyn ferrofin_extensions::fingerprint::Fingerprinter>
        });
    // The Merge Versions extension's bulk merge/split service — shared by its
    // scheduled tasks (via the context below) and the `/MergeVersions/*` routes
    // (via `with_merge_versions` on the app state).
    let merge_versions: Arc<dyn ferrofin_traits::merge_versions::MergeVersionsManager> = Arc::new(
        ferrofin_extensions::merge_versions::MergeVersionsService::new(
            Arc::clone(&item_repository) as Arc<_>,
            Arc::clone(&item_persistence_service) as Arc<_>,
            Arc::clone(&library),
            Arc::clone(&virtual_folders),
            Arc::clone(&plugins),
        ),
    );
    let extension_cx = ferrofin_extensions::ExtensionContext {
        library: Arc::clone(&library),
        media_segments: Arc::clone(&media_segments),
        plugins: Arc::clone(&plugins),
        fingerprinter,
        cache_dir: config.cache_dir.join("extensions"),
        merge_versions: Arc::clone(&merge_versions),
    };
    ferrofin_extensions::register_tasks(&extensions, &extension_cx, &task_manager);
    // Tier-1b WASM plugin tasks and event delivery. Tasks self-gate on the
    // plugin's enabled flag (the Tier-1a pattern); event delivery is
    // non-blocking (spawn + bounded per-plugin queue), so a slow guest can
    // never hold up publication.
    for task in wasm_host.scheduled_tasks(&plugins) {
        task_manager.register(task);
    }
    wasm_host.subscribe_events(&event_bus, &plugins);
    let collections: Arc<dyn ferrofin_traits::collections::CollectionManager> =
        Arc::new(FerrofinCollectionManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&linked_children_service),
        ));
    let playlists: Arc<dyn ferrofin_traits::collections::PlaylistManager> =
        Arc::new(FerrofinPlaylistManager::new(
            db.clone(),
            Arc::clone(&library),
            Arc::clone(&linked_children_service),
        ));

    // The full Jellyfin dashboard task set (Library + Maintenance categories),
    // now that every backing manager exists. Once registered, the scheduler
    // fires their default (or overridden) triggers.
    {
        use ferrofin_core::scheduled_tasks::library as lib_tasks;
        use ferrofin_core::scheduled_tasks::maintenance as maint_tasks;
        let paths_dyn: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
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
    let tasks: Arc<dyn ferrofin_traits::tasks::TaskManager> = Arc::new(task_manager.clone());
    // The trigger scheduler: fires startup triggers now, then evaluates
    // daily/weekly/interval triggers for the life of the process.
    drop(task_manager.start_scheduler());
    let _external_data: Arc<dyn ferrofin_traits::system::ExternalDataManager> =
        Arc::new(FerrofinExternalDataManager::new(
            Arc::clone(&path_manager),
            Arc::clone(&keyframe_repository),
            Arc::clone(&media_segments),
            Arc::clone(&trickplay),
            Arc::clone(&chapters),
        ));

    // ---- dto (consumes many of the above) ---------------------------------
    let dto: Arc<dyn ferrofin_traits::dto::DtoService> = Arc::new(FerrofinDtoService::new(
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
    let session_bus: Arc<dyn ferrofin_traits::session_bus::SessionMessageBus> =
        Arc::new(ferrofin_core::FerrofinSessionMessageBus::new());
    let sessions: Arc<dyn ferrofin_traits::session::SessionManager> = Arc::new(
        FerrofinSessionManager::new(
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

    // Forward domain events to client sessions over the WebSocket — the Rust
    // shape of Jellyfin's notifier entry points (`LibraryChangedNotifier`,
    // `ScheduledTaskEnded`, refresh progress). Consumers subscribe on the
    // concrete bus and spawn the async send so publication never blocks.
    {
        use ferrofin_model::session::SessionMessageType;
        let forward = |bus: &FerrofinEventManager,
                       event: &'static str,
                       message_type: SessionMessageType,
                       admin_only: bool| {
            let sessions = Arc::clone(&sessions);
            bus.subscribe(
                event,
                Arc::new(move |payload: &str| {
                    let sessions = Arc::clone(&sessions);
                    let payload = payload.to_owned();
                    tokio::spawn(async move {
                        let result = if admin_only {
                            sessions
                                .send_message_to_admin_sessions(message_type, &payload)
                                .await
                        } else {
                            sessions
                                .send_message_to_all_sessions(message_type, &payload)
                                .await
                        };
                        if let Err(err) = result {
                            tracing::debug!(%err, ?message_type, "failed to push event to sessions");
                        }
                    });
                    Ok(())
                }),
            );
        };
        // Library adds/removes → every signed-in client refreshes its views.
        forward(
            &event_bus,
            "LibraryChanged",
            SessionMessageType::LibraryChanged,
            false,
        );
        // Scan % + task completion → the admin dashboard's live displays.
        forward(
            &event_bus,
            "RefreshProgress",
            SessionMessageType::RefreshProgress,
            true,
        );
        forward(
            &event_bus,
            "TaskCompleted",
            SessionMessageType::ScheduledTaskEnded,
            true,
        );

        // Session start/end also land in the dashboard's activity feed — the
        // bulk of what Jellyfin's feed shows (port of the SessionStartedLogger
        // / SessionEndedLogger event consumers).
        let log_session_event =
            |bus: &FerrofinEventManager,
             event: &'static str,
             type_: &'static str,
             template: fn(&str, &str) -> String| {
                let activity = Arc::clone(&activity);
                bus.subscribe(
                    event,
                    Arc::new(move |payload: &str| {
                        let Ok(session) =
                            serde_json::from_str::<ferrofin_model::dto::SessionInfoDto>(payload)
                        else {
                            return Ok(());
                        };
                        // A session with no user (an API key client) writes nothing,
                        // matching upstream's user-scoped loggers.
                        let Some(user_name) = session.user_name.filter(|n| !n.is_empty()) else {
                            return Ok(());
                        };
                        let user_id = (!session.user_id.is_nil()).then_some(session.user_id);
                        let device = session.device_name.unwrap_or_default();
                        let entry = ferrofin_traits::activity::ActivityLogCreate {
                            name: template(&user_name, &device),
                            type_: type_.to_owned(),
                            user_id,
                            short_overview: session
                                .remote_end_point
                                .filter(|e| !e.is_empty())
                                .map(|endpoint| format!("IP address: {endpoint}")),
                            ..Default::default()
                        };
                        let activity = Arc::clone(&activity);
                        tokio::spawn(async move {
                            let _ = activity.create_entry(entry).await;
                        });
                        Ok(())
                    }),
                );
            };
        log_session_event(
            &event_bus,
            "SessionStarted",
            "SessionStarted",
            |user, device| format!("{user} is online from {device}"),
        );
        log_session_event(
            &event_bus,
            "SessionEnded",
            "SessionEnded",
            |user, device| format!("{user} has disconnected from {device}"),
        );

        // A scan that changed the library queues chapter-image extraction, so
        // enabling a library's "Extract chapter images" option and rescanning
        // produces thumbnails — upstream extracts them as part of the item's
        // metadata refresh, where Ferrofin has only the nightly task. This is
        // that same task: idempotent (existing images are reused), a no-op
        // when no library enables the option, and the task manager coalesces a
        // queue request arriving while it already runs.
        {
            let task_manager = task_manager.clone();
            let folders = Arc::clone(&virtual_folders);
            event_bus.subscribe(
                "LibraryChanged",
                Arc::new(move |_payload: &str| {
                    let task_manager = task_manager.clone();
                    let folders = Arc::clone(&folders);
                    tokio::spawn(async move {
                        let wanted = folders.get_virtual_folders().await.is_ok_and(|list| {
                            list.iter().any(|f| {
                                f.library_options
                                    .as_ref()
                                    .is_some_and(|o| o.enable_chapter_image_extraction)
                            })
                        });
                        if wanted {
                            let _ = task_manager.queue("RefreshChapterImages");
                        }
                    });
                    Ok(())
                }),
            );
        }
    }

    // These two maintenance tasks gate on active playback, so they register
    // once the session manager exists (registration order is otherwise inert).
    {
        use ferrofin_core::scheduled_tasks::maintenance as maint_tasks;
        let paths_dyn: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
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
    let tv_series: Arc<dyn ferrofin_traits::tv::TvSeriesManager> =
        Arc::new(FerrofinTvSeriesManager::new(
            Arc::clone(&users),
            Arc::clone(&library),
            Arc::clone(&next_up_service),
            Arc::clone(&dto),
            Arc::clone(&config_trait),
        ));
    // Arm the guest capabilities (query-items / write-media-segments /
    // next-up) now that every backing manager exists — plugin load happened
    // earlier, so a guest calling these during load got a clean
    // "not available" error, and the server is not yet serving, so nothing
    // can race the arming.
    wasm_host.set_runtime_collaborators(ferrofin_wasm::capabilities::Collaborators {
        handle: tokio::runtime::Handle::current(),
        library: Arc::clone(&library),
        media_segments: Arc::clone(&media_segments),
        plugins: Arc::clone(&plugins),
        users: Arc::clone(&users),
        user_data: Arc::clone(&user_data),
        tv: Arc::clone(&tv_series),
        media_streams: Arc::clone(&media_stream_repository),
        lyrics: Arc::clone(&lyrics),
        subtitles: Arc::clone(&subtitles),
        collections: Arc::clone(&collections),
        extractor: Arc::new(ferrofin_mediaencoding::FfmpegMediaExtractor::new(
            ffmpeg.ffmpeg.to_string_lossy().into_owned(),
        )),
        // Global decode budget for plugin analysis — operator-tunable
        // (FERROFIN_WASM_ANALYSIS_CONCURRENCY); default a quarter of the
        // cores, at least one: analysis must never starve transcodes.
        analysis: Arc::new(tokio::sync::Semaphore::new(
            config
                .wasm_analysis_concurrency
                .filter(|n| *n > 0)
                .map_or_else(num_cpus_for_analysis, |n| n as usize)
                .max(1),
        )),
    });
    // The analysis driver (offers new items to analyzer plugins) exists
    // only when some loaded plugin declares scan-targets.
    if let Some(task) = wasm_host.analysis_task(&plugins) {
        task_manager.register(task);
    }

    // ---- host + system + auth + quick-connect -----------------------------
    let app_host = Arc::new(FerrofinServerApplicationHost::new(
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
    let app_host_trait: Arc<dyn ferrofin_traits::system::ServerApplicationHost> =
        Arc::clone(&app_host) as Arc<_>;

    let system: Arc<dyn ferrofin_traits::system::SystemManager> = Arc::new(
        FerrofinSystemManager::new(
            Arc::clone(&app_host_trait),
            Arc::clone(&config_trait),
            Arc::clone(&paths),
            Arc::clone(&lifecycle),
            SystemHostFacts {
                // Report the emulated Jellyfin API version (clients gate on this), not
                // Ferrofin's own crate version — see JELLYFIN_API_VERSION.
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
    let auth_context_concrete = FerrofinAuthorizationContext::new(
        db.clone(),
        Arc::clone(&users),
        Arc::clone(&app_host_trait),
        Arc::clone(&config_trait),
        server_id.clone(),
        crate::service_version(),
    )
    .with_auth_cache(Arc::clone(&auth_cache));
    let auth_service: Arc<dyn ferrofin_traits::net::AuthService> =
        Arc::new(FerrofinAuthService::new(auth_context_concrete.clone()));
    let auth_context: Arc<dyn ferrofin_traits::net::AuthorizationContext> =
        Arc::new(auth_context_concrete);

    let quick_connect: Arc<dyn ferrofin_traits::security::QuickConnect> = Arc::new(
        FerrofinQuickConnect::new(Arc::clone(&config_trait), Arc::clone(&sessions)),
    );

    // Clone the collaborators the media-encoding pair needs before they are
    // moved into `AppState::new` below.
    let me_media_sources = Arc::clone(&media_sources);
    let me_media_encoder = Arc::clone(&media_encoder);
    let me_config = Arc::clone(&config_trait);
    let me_path_manager = Arc::clone(&path_manager);
    // The transcode planner resolves item/library display names for its logs.
    let me_library = Arc::clone(&library);

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
        // Transcode logs resolve item/series/library names through the library.
        Some(me_library),
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
    // host); the compiled-in plugin design is described in `docs/PLUGINS_UPSTREAM.md`.
    let state = state.with_plugins(Arc::clone(&plugins));

    // ---- SyncPlay ---------------------------------------------------------
    // The SyncPlay manager shares the session message bus (created with the
    // session manager above) to deliver group commands to member sockets.
    let sync_play: Arc<dyn ferrofin_traits::stubs::SyncPlayManager> = Arc::new(
        ferrofin_core::FerrofinSyncPlayManager::new(Arc::clone(&session_bus)),
    );
    // ---- File Transformation pipeline --------------------------------------
    // The registry the static `/web` mount consults per request. The Intro
    // Skipper's skip-button patch for `main.jellyfin.bundle.js` is its
    // compiled-in registration (the upstream plugin registers it via .NET
    // reflection); both transformers self-gate on their plugin's enabled flag
    // and configuration, so dashboard toggles apply live.
    let file_transformations: Arc<dyn ferrofin_traits::plugins::FileTransformationService> =
        Arc::new(
            ferrofin_extensions::file_transformation::WebFileTransformationService::new(
                Arc::clone(&plugins),
                format!("http://127.0.0.1:{}", config.port),
            ),
        );
    ferrofin_extensions::file_transformation::register_skip_button_transformer(
        file_transformations.as_ref(),
        Arc::clone(&plugins),
    )
    .await;
    // Runtime (WASM) plugins' declared web transforms — enabled plugins
    // only; the WIT trust note applies (client-side injection).
    wasm_host
        .register_web_transforms(&file_transformations, &plugins)
        .await;

    // ---- playback-decision metrics (feeds the benchmark suite) -------------
    let playback_metrics: Arc<dyn ferrofin_traits::metrics::PlaybackMetrics> =
        Arc::new(ferrofin_core::FerrofinPlaybackMetrics::new(db.clone()));

    // The runtime plugins' URL space (`/Plugins/{id}/web/…`).
    let plugin_routes: Arc<dyn ferrofin_traits::plugins::PluginRequestHandler> = Arc::new(
        ferrofin_wasm::WasmRequestDispatcher::new(&wasm_host, Arc::clone(&plugins)),
    );
    let state = state
        .with_plugin_request_handler(plugin_routes)
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
            port: 8096,
            https_port: 8920,
            ..Config::test_stub(root)
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
        let _router = ferrofin_api::create_router(wired.state.clone());
        // The host starts un-flagged; the composition root flips it after mount.
        wired.app_host.mark_core_startup_complete();
    }

    #[tokio::test]
    async fn lifecycle_controller_signals_shutdown_and_flags_restart() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let lifecycle = FerrofinLifecycleController::new(tx);
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
