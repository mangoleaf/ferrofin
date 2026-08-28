//! [`AppState`] — the dependency-injection seam shared by every handler.
//!
//! Handlers depend only on the `ferrofin-traits` manager traits, held here as
//! `Arc<dyn Trait>`. The concrete implementations are wired at the composition
//! root (`ferrofin-server`, Wave 8); `ferrofin-api` never names `ferrofin-core`. Tests
//! inject small fake trait impls instead.
//!
//! [`AppState`] is a thin `Arc<`[`Inner`]`>` newtype so it is cheap to
//! [`Clone`] into every axum handler (axum requires `State` to be `Clone`).

use std::sync::Arc;

use ferrofin_traits::activity::ActivityManager;
use ferrofin_traits::collections::{CollectionManager, PlaylistManager};
use ferrofin_traits::configuration::{DisplayPreferencesManager, ServerConfigurationManager};
use ferrofin_traits::devices::DeviceManager;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::events::ClientEventLogger;
use ferrofin_traits::filesystem::FileSystem;
use ferrofin_traits::library::{
    LibraryManager, LibraryMonitor, MediaSourceManager, MusicManager, SearchManager,
    SimilarItemsManager, UserDataManager, UserManager, UserViewManager, VirtualFolderManager,
};
use ferrofin_traits::localization::LocalizationManager;
use ferrofin_traits::media_encoding::{AttachmentExtractor, HlsStreamManager, SubtitleEncoder};
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::net::{AuthService, AuthorizationContext};
use ferrofin_traits::plugins::{DisabledPluginManager, PluginManager};
use ferrofin_traits::providers::ProviderManager;
use ferrofin_traits::security::{ApiKeyManager, QuickConnect};
use ferrofin_traits::session::SessionManager;
use ferrofin_traits::stubs::DisabledHlsStreamManager;
use ferrofin_traits::stubs::DisabledSubtitleEncoder;
use ferrofin_traits::stubs::DisabledVirtualFolderManager;
use ferrofin_traits::stubs::LyricManager;
use ferrofin_traits::stubs::NoopLibraryMonitor;
use ferrofin_traits::subtitles::SubtitleManager;
use ferrofin_traits::system::{ServerApplicationHost, SystemManager};
use ferrofin_traits::tasks::TaskManager;
use ferrofin_traits::trickplay::TrickplayManager;
use ferrofin_traits::tv::TvSeriesManager;

/// C# `GetNormalizedRemoteIP`: an IPv4-mapped IPv6 peer is compared as the IPv4
/// address it actually is.
#[must_use]
pub fn normalize_ip(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, std::net::IpAddr::V4),
        std::net::IpAddr::V4(_) => ip,
    }
}

/// The `X-Forwarded-For` chain, left to right as sent, keeping only entries that
/// parse as addresses.
///
/// A port may be appended to an entry (`203.0.113.9:41234`), and an IPv6 entry
/// may be bracketed; both spellings are read. An entry that is neither is
/// dropped rather than ending the walk, since a proxy that writes an obfuscated
/// identifier should not silently pin the client to the hop before it.
#[must_use]
pub fn forwarded_for(headers: &axum::http::HeaderMap) -> Vec<std::net::IpAddr> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|entry| parse_forwarded_entry(entry.trim()))
        .collect()
}

/// One `X-Forwarded-For` entry as an address.
#[must_use]
fn parse_forwarded_entry(entry: &str) -> Option<std::net::IpAddr> {
    if let Ok(ip) = entry.parse::<std::net::IpAddr>() {
        return Some(normalize_ip(ip));
    }
    // `host:port`, or `[v6]:port`.
    if let Ok(socket) = entry.parse::<std::net::SocketAddr>() {
        return Some(normalize_ip(socket.ip()));
    }
    let trimmed = entry.trim_start_matches('[').trim_end_matches(']');
    trimmed.parse::<std::net::IpAddr>().ok().map(normalize_ip)
}

/// The managers behind [`AppState`], held once and shared via [`Arc`].
///
/// One field per `ferrofin-traits` manager the API layer calls. Each is a trait
/// object so the concrete type is chosen at the composition root, not baked into
/// this crate.
pub struct Inner {
    /// The network policy — which peers count as local, and which remote ones
    /// may reach the server at all (`RemoteIPFilter`).
    ///
    /// `None` in tests and until the composition root wires it, in which case
    /// the private-range fallback in `handlers::system::is_in_local_network`
    /// applies and no remote filter is enforced. Behind a lock because
    /// `NetworkManager::update_settings` rewrites its caches when an admin
    /// saves the network configuration.
    pub network: Option<Arc<std::sync::RwLock<ferrofin_networking::NetworkManager>>>,
    /// Library catalogue queries and item resolution.
    pub library: Arc<dyn LibraryManager>,
    /// User accounts, authentication policy, and profiles.
    pub users: Arc<dyn UserManager>,
    /// A user's home-screen views (folders, collections, latest).
    pub user_views: Arc<dyn UserViewManager>,
    /// Per-user playback state (played flags, resume positions, favourites).
    pub user_data: Arc<dyn UserDataManager>,
    /// Playable media sources and stream selection for an item.
    pub media_sources: Arc<dyn MediaSourceManager>,
    /// Active client sessions and playback reporting.
    pub sessions: Arc<dyn SessionManager>,
    /// System information, restart/shutdown, and logs.
    pub system: Arc<dyn SystemManager>,
    /// The hosting application (URLs, capabilities, environment).
    pub app_host: Arc<dyn ServerApplicationHost>,
    /// Server configuration read/write.
    pub config: Arc<dyn ServerConfigurationManager>,
    /// Metadata/image refresh orchestration (queueing item refreshes).
    pub providers: Arc<dyn ProviderManager>,
    /// On-the-fly image resize/format conversion for image serving. `None` until
    /// the composition root wires a concrete processor (image routes then serve the
    /// stored original untransformed).
    pub image_processor: Option<Arc<dyn ferrofin_traits::drawing::ImageProcessor>>,
    /// Builds "instant mix" playlists from a seed song/album/artist/genre.
    pub music: Arc<dyn MusicManager>,
    /// Finds items similar to a seed and builds recommendation categories.
    pub similar_items: Arc<dyn SimilarItemsManager>,
    /// Ranked search-hint queries across the library.
    pub search: Arc<dyn SearchManager>,
    /// Builds the wire DTOs returned to clients from domain entities.
    pub dto: Arc<dyn DtoService>,
    /// Parses a request's credentials into an authorization context.
    pub auth_context: Arc<dyn AuthorizationContext>,
    /// Validates a request's credentials, rejecting unauthenticated ones.
    pub auth_service: Arc<dyn AuthService>,
    /// Drives the Quick Connect pairing flow.
    pub quick_connect: Arc<dyn QuickConnect>,
    /// Creates and mutates playlists and their shares/membership.
    pub playlists: Arc<dyn PlaylistManager>,
    /// Creates and mutates collections (box sets) and their membership.
    pub collections: Arc<dyn CollectionManager>,
    /// Computes a user's "Next Up" TV-episode queue.
    pub tv_series: Arc<dyn TvSeriesManager>,
    /// Searches/uploads/deletes an item's subtitles.
    pub subtitles: Arc<dyn SubtitleManager>,
    /// Serves/searches/uploads an audio item's lyrics.
    pub lyrics: Arc<dyn LyricManager>,
    /// Queries an item's media segments (intros/outros/etc.).
    pub media_segments: Arc<dyn MediaSegmentManager>,
    /// Serves trickplay (scrubbing-preview) playlists and tiles.
    pub trickplay: Arc<dyn TrickplayManager>,
    /// Registers client devices and their per-device options.
    pub devices: Arc<dyn DeviceManager>,
    /// Persists client-uploaded diagnostic documents.
    pub client_event_logger: Arc<dyn ClientEventLogger>,
    /// Lists, creates, and revokes long-lived server API keys.
    pub api_keys: Arc<dyn ApiKeyManager>,
    /// Culture/country/parental-rating reference data.
    pub localization: Arc<dyn LocalizationManager>,
    /// Per-user, per-client display preferences.
    pub display_preferences: Arc<dyn DisplayPreferencesManager>,
    /// Paged retrieval of server activity-log entries.
    pub activity: Arc<dyn ActivityManager>,
    /// Server-side filesystem browsing (Environment endpoints).
    pub file_system: Arc<dyn FileSystem>,
    /// Enumerates and runs the server's scheduled tasks.
    pub tasks: Arc<dyn TaskManager>,
    /// The dynamic-HLS + transcode-stream runtime (playlists, segments, the
    /// transcode branch of `/Videos|Audio/{id}/stream`). Defaults to the
    /// disabled stub; the composition root injects the ffmpeg-backed impl.
    pub hls: Arc<dyn HlsStreamManager>,
    /// Extracts embedded attachments (fonts/covers) for the
    /// `Videos/{id}/{source}/Attachments/{index}` route. Defaults to the
    /// disabled stub; the composition root injects the ffmpeg-backed impl.
    pub attachments: Arc<dyn AttachmentExtractor>,
    /// Converts subtitle tracks on the fly (charset-normalize + reformat) for the
    /// `Videos/{id}/{source}/Subtitles/{index}/{format}` + HLS-playlist routes.
    /// Defaults to the disabled stub; the composition root injects the
    /// ffmpeg-backed `SubtitleEncoderImpl`.
    pub subtitle_encoder: Arc<dyn SubtitleEncoder>,
    /// The on-disk virtual-folder (library-structure) admin surface backing
    /// `/Library/VirtualFolders*` and `/Library/PhysicalPaths`. Defaults to the
    /// disabled stub (empty reads, rejected writes); the composition root injects
    /// the filesystem-backed `FerrofinVirtualFolderManager` rooted at
    /// `DefaultUserViewsPath` via [`AppState::with_virtual_folders`].
    pub virtual_folders: Arc<dyn VirtualFolderManager>,

    /// The filesystem-change monitor backing the external-source change-report
    /// webhooks (`/Library/Movies/*`, `/Library/Series/*`,
    /// `/Library/Media/Updated`). Defaults to the no-op monitor (change reports
    /// are validated and logged, succeed, and touch no filesystem); the
    /// composition root injects the watcher-backed `FerrofinLibraryMonitor` via
    /// [`AppState::with_library_monitor`].
    pub library_monitor: Arc<dyn LibraryMonitor>,

    /// The Tier-1 (compile-time) plugin manager backing `/Plugins/*`,
    /// `/Packages/*` and `/Repositories`. Defaults to the disabled stub (no
    /// plugins, no repositories, mutators rejected); the composition root injects
    /// the registry-backed `FerrofinPluginManager` via [`AppState::with_plugins`].
    pub plugins: Arc<dyn PluginManager>,

    /// Synchronized group playback (`/SyncPlay/*`). `None` until the composition
    /// root wires a manager via [`AppState::with_sync_play`]; the SyncPlay routes
    /// return `501` while unset.
    pub sync_play: Option<Arc<dyn ferrofin_traits::stubs::SyncPlayManager>>,

    /// The server→client WebSocket message bus (session-socket registry). `None`
    /// until the composition root wires it via [`AppState::with_session_bus`]; the
    /// session socket then registers/unregisters its sink here so SyncPlay (and
    /// future now-playing/remote-control pushes) can reach the client.
    pub session_bus: Option<Arc<dyn ferrofin_traits::session_bus::SessionMessageBus>>,

    /// Live TV (`/LiveTv/*`). `None` until the composition root wires the real
    /// manager via [`AppState::with_live_tv`]; while unset the Live TV routes
    /// report the disabled/empty state.
    pub live_tv: Option<Arc<dyn ferrofin_traits::stubs::LiveTvManager>>,

    /// The web-file transformation pipeline (the File Transformation
    /// extension). `None` until the composition root wires it via
    /// [`AppState::with_file_transformations`]; while unset a transformation
    /// registration is accepted and dropped (nothing serves transformable
    /// files without the composition root's web mount anyway).
    pub file_transformations: Option<Arc<dyn ferrofin_traits::plugins::FileTransformationService>>,
    /// Dispatches `/Plugins/{id}/web/…` requests into the owning runtime
    /// plugin. `None` (no WASM host wired) ⇒ those routes 404.
    pub plugin_routes: Option<Arc<dyn ferrofin_traits::plugins::PluginRequestHandler>>,

    /// The playback-decision metrics recorder (feeds the benchmark suite's
    /// playback metrics). `None` until the composition root wires it via
    /// [`AppState::with_playback_metrics`]; while unset decisions are simply
    /// not recorded (recording is observability, never load-bearing).
    pub playback_metrics: Option<Arc<dyn ferrofin_traits::metrics::PlaybackMetrics>>,

    /// The Merge Versions extension's bulk merge/split service backing the
    /// `/MergeVersions/*` routes. `None` until the composition root wires it
    /// via [`AppState::with_merge_versions`]; while unset those routes report
    /// the plugin unavailable (`404`, like a Jellyfin server without the
    /// plugin's controller).
    pub merge_versions: Option<Arc<dyn ferrofin_traits::merge_versions::MergeVersionsManager>>,
}

/// The shared application state passed to every axum handler as
/// [`axum::extract::State`].
///
/// Cloning an [`AppState`] clones a single [`Arc`], so it is cheap to hand to
/// each route. Construct one with [`AppState::new`] (or [`AppState::from_inner`])
/// at the composition root.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

impl AppState {
    /// Wraps an already-assembled [`Inner`] set of managers.
    #[must_use]
    pub fn from_inner(inner: Inner) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Builds an [`AppState`] from each manager trait object.
    ///
    /// The composition root passes the concrete `ferrofin-core` impls (as
    /// `Arc<dyn Trait>`); tests pass fakes. The argument order matches the field
    /// order of [`Inner`].
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        library: Arc<dyn LibraryManager>,
        users: Arc<dyn UserManager>,
        user_views: Arc<dyn UserViewManager>,
        user_data: Arc<dyn UserDataManager>,
        media_sources: Arc<dyn MediaSourceManager>,
        sessions: Arc<dyn SessionManager>,
        system: Arc<dyn SystemManager>,
        app_host: Arc<dyn ServerApplicationHost>,
        config: Arc<dyn ServerConfigurationManager>,
        providers: Arc<dyn ProviderManager>,
        music: Arc<dyn MusicManager>,
        similar_items: Arc<dyn SimilarItemsManager>,
        search: Arc<dyn SearchManager>,
        dto: Arc<dyn DtoService>,
        auth_context: Arc<dyn AuthorizationContext>,
        auth_service: Arc<dyn AuthService>,
        quick_connect: Arc<dyn QuickConnect>,
        playlists: Arc<dyn PlaylistManager>,
        collections: Arc<dyn CollectionManager>,
        tv_series: Arc<dyn TvSeriesManager>,
        subtitles: Arc<dyn SubtitleManager>,
        lyrics: Arc<dyn LyricManager>,
        media_segments: Arc<dyn MediaSegmentManager>,
        trickplay: Arc<dyn TrickplayManager>,
        devices: Arc<dyn DeviceManager>,
        client_event_logger: Arc<dyn ClientEventLogger>,
        api_keys: Arc<dyn ApiKeyManager>,
        localization: Arc<dyn LocalizationManager>,
        display_preferences: Arc<dyn DisplayPreferencesManager>,
        activity: Arc<dyn ActivityManager>,
        file_system: Arc<dyn FileSystem>,
        tasks: Arc<dyn TaskManager>,
    ) -> Self {
        Self::from_inner(Inner {
            // Wired by the composition root via `with_network`.
            network: None,
            library,
            users,
            user_views,
            user_data,
            media_sources,
            sessions,
            system,
            app_host,
            config,
            providers,
            image_processor: None,
            sync_play: None,
            session_bus: None,
            live_tv: None,
            file_transformations: None,
            plugin_routes: None,
            playback_metrics: None,
            merge_versions: None,
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
            // Default to the disabled stubs; a host with a transcode runtime
            // overrides them via `with_media_encoding` at the composition root.
            hls: Arc::new(DisabledHlsStreamManager),
            attachments: Arc::new(ferrofin_traits::stubs::DisabledAttachmentExtractor),
            subtitle_encoder: Arc::new(DisabledSubtitleEncoder),
            // Default to the disabled virtual-folder store; the composition root
            // overrides it via `with_virtual_folders`.
            virtual_folders: Arc::new(DisabledVirtualFolderManager),
            // Default to the no-op library monitor; the composition root overrides
            // it via `with_library_monitor` once the filesystem watcher is wired.
            library_monitor: Arc::new(NoopLibraryMonitor),
            // Default to the disabled plugin manager; the composition root injects
            // the registry-backed `FerrofinPluginManager` via `with_plugins`.
            plugins: Arc::new(DisabledPluginManager),
        })
    }

    /// Replaces the media-encoding seams (HLS/transcode runtime + attachment
    /// extractor) with concrete implementations.
    ///
    /// [`new`](Self::new) installs the disabled stubs so the many test
    /// constructors keep compiling; the composition root calls this to wire the
    /// ffmpeg-backed [`HlsStreamManager`] and [`AttachmentExtractor`] before the
    /// state is shared. Panics only if called after the state has been cloned
    /// (the inner `Arc` is still uniquely held here at construction time).
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned) — only valid to call
    /// at the composition root before the router is built.
    #[must_use]
    pub fn with_media_encoding(
        mut self,
        hls: Arc<dyn HlsStreamManager>,
        attachments: Arc<dyn AttachmentExtractor>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_media_encoding must be called before the state is shared");
        inner.hls = hls;
        inner.attachments = attachments;
        self
    }

    /// Replaces the subtitle encoder with a concrete implementation.
    ///
    /// [`new`](Self::new) installs the disabled stub (every conversion reports
    /// "subtitle conversion is not available") so the test constructors keep
    /// compiling; the composition root calls this to wire the ffmpeg-backed
    /// `SubtitleEncoderImpl` before the state is shared.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned) — only valid to call
    /// at the composition root before the router is built.
    #[must_use]
    pub fn with_subtitle_encoder(mut self, subtitle_encoder: Arc<dyn SubtitleEncoder>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_subtitle_encoder must be called before the state is shared");
        inner.subtitle_encoder = subtitle_encoder;
        self
    }

    /// Replaces the virtual-folder store with a concrete implementation.
    ///
    /// [`new`](Self::new) installs the disabled stub (empty reads, rejected
    /// writes) so the many test constructors keep compiling; the composition root
    /// calls this to wire the filesystem-backed
    /// `FerrofinVirtualFolderManager` (rooted at `DefaultUserViewsPath`) before the
    /// state is shared.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned) — only valid to call
    /// at the composition root before the router is built.
    #[must_use]
    pub fn with_virtual_folders(mut self, virtual_folders: Arc<dyn VirtualFolderManager>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_virtual_folders must be called before the state is shared");
        inner.virtual_folders = virtual_folders;
        self
    }

    /// Replaces the library monitor with a concrete implementation.
    ///
    /// [`new`](Self::new) installs the no-op monitor (change reports succeed and
    /// are logged, no filesystem is touched) so every test constructor keeps
    /// compiling; the composition root calls this to wire the watcher-backed
    /// `FerrofinLibraryMonitor` before the state is shared.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned) — only valid to call
    /// at the composition root before the router is built.
    #[must_use]
    pub fn with_library_monitor(mut self, library_monitor: Arc<dyn LibraryMonitor>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_library_monitor must be called before the state is shared");
        inner.library_monitor = library_monitor;
        self
    }

    /// Replaces the plugin manager with a concrete implementation.
    ///
    /// [`new`](Self::new) installs the disabled stub (no plugins, no repositories,
    /// mutators rejected) so the many test constructors keep compiling; the
    /// composition root calls this to wire the registry-backed
    /// `FerrofinPluginManager` before the state is shared.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned) — only valid to call
    /// at the composition root before the router is built.
    #[must_use]
    pub fn with_plugins(mut self, plugins: Arc<dyn PluginManager>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_plugins must be called before the state is shared");
        inner.plugins = plugins;
        self
    }

    /// Wires the on-the-fly image processor used by the image-serving routes.
    ///
    /// [`new`](Self::new) leaves it `None` (image routes serve the stored original
    /// untransformed); the composition root calls this with the real
    /// `image`-crate-backed processor so `maxWidth`/`format`/… requests resize.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned).
    #[must_use]
    pub fn with_image_processor(
        mut self,
        image_processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_image_processor must be called before the state is shared");
        inner.image_processor = Some(image_processor);
        self
    }

    /// Wires the SyncPlay manager backing the `/SyncPlay/*` routes.
    ///
    /// [`new`](Self::new) leaves it unset (those routes return `501`); the
    /// composition root calls this with the real group-coordinating manager.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned).
    #[must_use]
    pub fn with_sync_play(
        mut self,
        sync_play: Arc<dyn ferrofin_traits::stubs::SyncPlayManager>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_sync_play must be called before the state is shared");
        inner.sync_play = Some(sync_play);
        self
    }

    /// Wires the real Live TV manager.
    ///
    /// [`new`](Self::new) leaves it unset (Live TV reports disabled/empty); the
    /// composition root calls this with the `ferrofin-livetv` implementation.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned).
    #[must_use]
    pub fn with_live_tv(mut self, live_tv: Arc<dyn ferrofin_traits::stubs::LiveTvManager>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_live_tv must be called before the state is shared");
        inner.live_tv = Some(live_tv);
        self
    }

    /// Wires the network policy so the local-network test and the remote-IP
    /// filter are the CONFIGURED ones rather than a private-range guess.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned).
    #[must_use]
    pub fn with_network(
        mut self,
        network: Arc<std::sync::RwLock<ferrofin_networking::NetworkManager>>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_network must be called before the state is shared");
        inner.network = Some(network);
        self
    }

    /// The address of the client that actually made `parts`' request.
    ///
    /// The transport peer, unless `KnownProxies` is configured and the peer is
    /// one — then the `X-Forwarded-For` chain is walked (see
    /// [`ferrofin_networking::NetworkManager::client_address`]). Behind a
    /// reverse proxy this is the difference between every request looking like
    /// it came from the ingress and the policy seeing who is really calling.
    ///
    /// Falls back to loopback when there is no peer at all, exactly as C#
    /// `GetNormalizedRemoteIP` defaults it.
    #[must_use]
    pub fn client_address(&self, parts: &axum::http::request::Parts) -> std::net::IpAddr {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), |ci| {
                normalize_ip(ci.0.ip())
            });
        self.client_address_for(peer, &forwarded_for(&parts.headers))
    }

    /// [`Self::client_address`] over an already-extracted peer and chain, so the
    /// middleware (which holds a `Request`, not `Parts`) shares the one rule.
    #[must_use]
    pub fn client_address_for(
        &self,
        peer: std::net::IpAddr,
        forwarded_for: &[std::net::IpAddr],
    ) -> std::net::IpAddr {
        match self.inner.network.as_ref() {
            Some(network) => network.read().map_or_else(
                |e| e.into_inner().client_address(peer, forwarded_for),
                |n| n.client_address(peer, forwarded_for),
            ),
            // No policy wired means no known proxies, and upstream ignores the
            // header entirely in that case.
            None => peer,
        }
    }

    /// Whether `ip` is on the local network.
    ///
    /// The configured answer (`NetworkManager::IsInLocalNetwork`, which
    /// intersects `LocalNetworkSubnets` and subtracts the `!`-prefixed
    /// exclusions) when the policy is wired; otherwise the private-range
    /// fallback, which is what the whole server used before.
    #[must_use]
    pub fn is_in_local_network(&self, ip: std::net::IpAddr) -> bool {
        match self.inner.network.as_ref() {
            Some(network) => network.read().map_or_else(
                |e| e.into_inner().is_in_local_network(ip),
                |n| n.is_in_local_network(ip),
            ),
            None => crate::handlers::system::is_in_local_network(ip),
        }
    }

    /// Whether a request from `ip` may be served at all — C#
    /// `NetworkManager.ShouldAllowServerAccess`, the `RemoteIPFilter` /
    /// `EnableRemoteAccess` gate. `Allow` when no policy is wired.
    #[must_use]
    pub fn remote_access_policy(
        &self,
        ip: std::net::IpAddr,
    ) -> ferrofin_networking::RemoteAccessPolicyResult {
        match self.inner.network.as_ref() {
            Some(network) => network.read().map_or_else(
                |e| e.into_inner().should_allow_server_access(ip),
                |n| n.should_allow_server_access(ip),
            ),
            None => ferrofin_networking::RemoteAccessPolicyResult::Allow,
        }
    }

    /// Re-reads the network configuration into the policy after an admin saves
    /// it, so a changed `LocalNetworkSubnets` / `RemoteIPFilter` takes effect
    /// without a restart (C# `NetworkManager` subscribes to the config event).
    pub fn update_network_settings(&self, config: &ferrofin_networking::NetworkConfiguration) {
        if let Some(network) = self.inner.network.as_ref() {
            match network.write() {
                Ok(mut n) => n.update_settings(config),
                Err(poisoned) => poisoned.into_inner().update_settings(config),
            }
        }
    }

    /// Wires the web-file transformation pipeline (the File Transformation
    /// extension), shared with the static web mount so registrations made via
    /// `POST /FileTransformation/RegisterTransformation` apply to served files.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned).
    #[must_use]
    pub fn with_file_transformations(
        mut self,
        service: Arc<dyn ferrofin_traits::plugins::FileTransformationService>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_file_transformations must be called before the state is shared");
        inner.file_transformations = Some(service);
        self
    }

    /// Injects the plugin-request dispatcher (the WASM host's URL space).
    ///
    /// # Panics
    /// If called after the state has been shared (composition-root only).
    #[must_use]
    pub fn with_plugin_request_handler(
        mut self,
        handler: Arc<dyn ferrofin_traits::plugins::PluginRequestHandler>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_plugin_request_handler must be called before the state is shared");
        inner.plugin_routes = Some(handler);
        self
    }

    /// Wires the session message bus the WebSocket handler registers sinks on.
    ///
    /// [`new`](Self::new) leaves it unset (server→client pushes are dropped); the
    /// composition root calls this with the concrete bus, shared with the SyncPlay
    /// manager so its commands reach connected sockets.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned).
    #[must_use]
    pub fn with_session_bus(
        mut self,
        session_bus: Arc<dyn ferrofin_traits::session_bus::SessionMessageBus>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_session_bus must be called before the state is shared");
        inner.session_bus = Some(session_bus);
        self
    }

    /// Injects the playback-decision metrics recorder.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned).
    #[must_use]
    pub fn with_playback_metrics(
        mut self,
        playback_metrics: Arc<dyn ferrofin_traits::metrics::PlaybackMetrics>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_playback_metrics must be called before the state is shared");
        inner.playback_metrics = Some(playback_metrics);
        self
    }

    /// Wires the Merge Versions extension's bulk merge/split service.
    ///
    /// [`new`](Self::new) leaves it unset (the `/MergeVersions/*` routes report
    /// the plugin unavailable); the composition root calls this with the
    /// `ferrofin-extensions` implementation.
    ///
    /// # Panics
    ///
    /// Panics if the inner state is already shared (cloned).
    #[must_use]
    pub fn with_merge_versions(
        mut self,
        merge_versions: Arc<dyn ferrofin_traits::merge_versions::MergeVersionsManager>,
    ) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_merge_versions must be called before the state is shared");
        inner.merge_versions = Some(merge_versions);
        self
    }

    /// The parsed-authorization context resolver.
    #[must_use]
    pub fn auth_context(&self) -> &Arc<dyn AuthorizationContext> {
        &self.inner.auth_context
    }

    /// The credential-validating authentication service.
    #[must_use]
    pub fn auth_service(&self) -> &Arc<dyn AuthService> {
        &self.inner.auth_service
    }
}

impl std::ops::Deref for AppState {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
