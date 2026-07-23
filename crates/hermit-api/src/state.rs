//! [`AppState`] — the dependency-injection seam shared by every handler.
//!
//! Handlers depend only on the `hermit-traits` manager traits, held here as
//! `Arc<dyn Trait>`. The concrete implementations are wired at the composition
//! root (`hermit-server`, Wave 8); `hermit-api` never names `hermit-core`. Tests
//! inject small fake trait impls instead.
//!
//! [`AppState`] is a thin `Arc<`[`Inner`]`>` newtype so it is cheap to
//! [`Clone`] into every axum handler (axum requires `State` to be `Clone`).

use std::sync::Arc;

use hermit_traits::activity::ActivityManager;
use hermit_traits::collections::{CollectionManager, PlaylistManager};
use hermit_traits::configuration::{DisplayPreferencesManager, ServerConfigurationManager};
use hermit_traits::devices::DeviceManager;
use hermit_traits::dto::DtoService;
use hermit_traits::events::ClientEventLogger;
use hermit_traits::filesystem::FileSystem;
use hermit_traits::library::{
    LibraryManager, MediaSourceManager, MusicManager, SearchManager, SimilarItemsManager,
    UserDataManager, UserManager, UserViewManager,
};
use hermit_traits::localization::LocalizationManager;
use hermit_traits::media_encoding::{AttachmentExtractor, HlsStreamManager};
use hermit_traits::media_segments::MediaSegmentManager;
use hermit_traits::net::{AuthService, AuthorizationContext};
use hermit_traits::providers::ProviderManager;
use hermit_traits::security::{ApiKeyManager, QuickConnect};
use hermit_traits::session::SessionManager;
use hermit_traits::stubs::DisabledHlsStreamManager;
use hermit_traits::stubs::LyricManager;
use hermit_traits::subtitles::SubtitleManager;
use hermit_traits::system::{ServerApplicationHost, SystemManager};
use hermit_traits::tasks::TaskManager;
use hermit_traits::trickplay::TrickplayManager;
use hermit_traits::tv::TvSeriesManager;

/// The managers behind [`AppState`], held once and shared via [`Arc`].
///
/// One field per `hermit-traits` manager the API layer calls. Each is a trait
/// object so the concrete type is chosen at the composition root, not baked into
/// this crate.
pub struct Inner {
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
    /// The composition root passes the concrete `hermit-core` impls (as
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
            attachments: Arc::new(hermit_traits::stubs::DisabledAttachmentExtractor),
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
