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

use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::dto::DtoService;
use hermit_traits::library::{
    LibraryManager, MediaSourceManager, UserDataManager, UserManager, UserViewManager,
};
use hermit_traits::net::{AuthService, AuthorizationContext};
use hermit_traits::session::SessionManager;
use hermit_traits::system::{ServerApplicationHost, SystemManager};

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
    /// Builds the wire DTOs returned to clients from domain entities.
    pub dto: Arc<dyn DtoService>,
    /// Parses a request's credentials into an authorization context.
    pub auth_context: Arc<dyn AuthorizationContext>,
    /// Validates a request's credentials, rejecting unauthenticated ones.
    pub auth_service: Arc<dyn AuthService>,
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
        dto: Arc<dyn DtoService>,
        auth_context: Arc<dyn AuthorizationContext>,
        auth_service: Arc<dyn AuthService>,
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
            dto,
            auth_context,
            auth_service,
        })
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
