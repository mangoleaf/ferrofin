//! Core manager implementations for Hermit — the workhorse.
//!
//! Port of `Emby.Server.Implementations` + `Jellyfin.Server.Implementations`.
//! Provides the concrete implementations of the `hermit-traits` manager traits:
//! item repository (the `InternalItemsQuery` → SQL builder over `hermit-db`),
//! library scanning, user/auth, sessions, DTO assembly (entity → `BaseItemDto`),
//! media sources, config/system/devices, plus the `BaseItem` domain behavior as
//! free functions over `BaseItemKind`.
//!
//! Sibling managers (MediaEncoder, ImageProcessor, ProviderManager, …) are taken
//! as `Arc<dyn Trait>` and injected at the composition root (`hermit-server`,
//! Wave 8) — this crate depends only on `hermit-traits` for them, not on the
//! impl crates. Peripheral + deferred-subsystem managers are minimal/stub.
//! Filled by the Wave 6 PortJob. See `brain/PLAN_HERMIT_PORT.md` + `brain/DEFERRED.md`.
//!
//! ## Unit 1 — item repository + query translation (First-Light core)
//!
//! This unit lands the foundation:
//! - [`kinds`] — the `BaseItem`/`Folder`/`Video` OOP behavior as free functions
//!   over [`hermit_model::data::BaseItemKind`];
//! - [`item_type_lookup`] — the static kind → stored-type-name tables
//!   ([`ItemTypeLookup`]);
//! - [`translate_query`] — the [`hermit_traits::options::InternalItemsQuery`] →
//!   SQL builder (a `sqlx::QueryBuilder` translator over `hermit-db`);
//! - [`item_repository`] — [`HermitItemRepository`] ([`hermit_traits::persistence::ItemRepository`]);
//! - [`item_count_service`] — [`HermitItemCountService`] ([`hermit_traits::persistence::ItemCountService`]);
//! - [`item_persistence_service`] — [`HermitItemPersistenceService`]
//!   ([`hermit_traits::persistence::ItemPersistenceService`]).
//!
//! ## Unit 2 — per-item sub-repositories
//!
//! Row-level CRUD for an item's child collections plus the two linked-item
//! services, all over `hermit-db` and reusing the unit-1 `kinds`/type-lookup
//! helpers and (for next-up) the stored `Type` names:
//! - [`chapter_repository`] — [`HermitChapterRepository`]
//!   ([`hermit_traits::persistence::ChapterRepository`]);
//! - [`media_stream_repository`] — [`HermitMediaStreamRepository`]
//!   ([`hermit_traits::persistence::MediaStreamRepository`]);
//! - [`media_attachment_repository`] — [`HermitMediaAttachmentRepository`]
//!   ([`hermit_traits::persistence::MediaAttachmentRepository`]);
//! - [`people_repository`] — [`HermitPeopleRepository`]
//!   ([`hermit_traits::persistence::PeopleRepository`]);
//! - [`keyframe_repository`] — [`HermitKeyframeRepository`]
//!   ([`hermit_traits::persistence::KeyframeRepository`]);
//! - [`linked_children_service`] — [`HermitLinkedChildrenService`]
//!   ([`hermit_traits::persistence::LinkedChildrenService`]);
//! - [`next_up_service`] — [`HermitNextUpService`]
//!   ([`hermit_traits::persistence::NextUpService`]), whose
//!   [`NextUpEpisodeBatchResult`](hermit_traits::persistence::NextUpEpisodeBatchResult)
//!   output is consumed by `TvSeriesManager` in a later unit.
//!
//! The shared [`db_error`] module holds the single `sqlx` → `ServiceError`
//! mapping and the `MediaStreamType` discriminant helper reused across units.
//!
//! ## Unit 5 — library manager, media sources, views, search, music
//!
//! The library-orchestration seam, all delegating to the unit 1–2 persistence
//! repositories (taken as `Arc<dyn _>`) rather than touching the pool directly:
//! - [`library_manager`] — [`HermitLibraryManager`]
//!   ([`hermit_traits::library::LibraryManager`]);
//! - [`media_source_manager`] — [`HermitMediaSourceManager`]
//!   ([`hermit_traits::library::MediaSourceManager`]), holding in-memory
//!   live-stream state and injecting `Arc<dyn MediaEncoder>` / `Arc<dyn
//!   ProviderManager>` for probing;
//! - [`user_view_manager`] — [`HermitUserViewManager`]
//!   ([`hermit_traits::library::UserViewManager`]);
//! - [`search_manager`] — [`HermitSearchManager`]
//!   ([`hermit_traits::library::SearchManager`]);
//! - [`music_manager`] — [`HermitMusicManager`]
//!   ([`hermit_traits::library::MusicManager`]);
//! - [`similar_items_manager`] — [`HermitSimilarItemsManager`]
//!   ([`hermit_traits::library::SimilarItemsManager`]);
//! - [`library_monitor`] — [`HermitLibraryMonitor`]
//!   ([`hermit_traits::library::LibraryMonitor`]), watching library filesystems
//!   behind a small [`resolvers::FileSystemWatcher`] abstraction so tests use
//!   fakes.
//!
//! The C# `BaseItem`/`Folder`/`Video` OOP tree that the C# library manager owns
//! lives as free functions in [`kinds`] and [`resolvers`] (path/name/sort
//! helpers) — this is where that logic lives.
//!
//! ## Unit 6 — DTO assembly service
//!
//! The presentation seam:
//! - [`dto_service`] — [`HermitDtoService`]
//!   ([`hermit_traits::dto::DtoService`]), the entity → [`BaseItemDto`](hermit_model::dto::BaseItemDto)
//!   assembly. It reads via the injected [`LibraryManager`](hermit_traits::library::LibraryManager) /
//!   [`UserDataManager`](hermit_traits::library::UserDataManager) /
//!   [`ItemCountService`](hermit_traits::persistence::ItemCountService),
//!   resolves images through an injected `Arc<dyn ImageProcessor>`, honors the
//!   [`DtoOptions`](hermit_traits::options::DtoOptions) field/image toggles, and
//!   reuses the [`kinds`] helpers to branch on an item's kind (the C#
//!   subclass type-tests). LiveTV program/channel enrichment and active-recording
//!   rewrites are deferred (their sibling seams are not injected into this unit).
//!
//! ## Unit 7 — session manager + WebSocket wiring
//!
//! The session/eventing seam:
//! - [`session_manager`] — [`HermitSessionManager`]
//!   ([`hermit_traits::session::SessionManager`]), the largest impl. Owns the
//!   in-memory `SessionInfo` table (the C# `_activeConnections`), reports
//!   playback through the injected [`UserDataManager`](hermit_traits::library::UserDataManager),
//!   and broadcasts server → client messages as pre-serialized JSON to each
//!   session's attached WebSocket connections. Sibling managers (user/device/
//!   user-data/library/DTO) plus the [`EventManager`](hermit_traits::events::EventManager)
//!   are injected; idle timers, instant-mix, and live-stream reference-counting
//!   are documented deferrals.
//! - [`event_manager`] — [`HermitEventManager`]
//!   ([`hermit_traits::events::EventManager`]), the in-process publish seam: a
//!   name-keyed [`EventConsumer`] registry replacing the C# DI-scoped
//!   `IEventConsumer<T>` lookup; publication logs-and-continues past a failing
//!   consumer.
//! - [`client_event_logger`] — [`HermitClientEventLogger`]
//!   ([`hermit_traits::events::ClientEventLogger`]), persisting client-uploaded
//!   diagnostic documents under the injected paths' log directory with a
//!   traversal-safe file name.
//! - [`session_websocket_listener`] — [`HermitSessionWebSocketListener`]
//!   ([`hermit_traits::net::WebSocketListener`]) resolving+attaching a
//!   connection to its session on connect, and [`HermitWebSocketManager`]
//!   ([`hermit_traits::net::WebSocketManager`]) validating the upgrade request.
//!   The actual HTTP → WS upgrade and per-connection receive loop belong to the
//!   HTTP layer (Wave 7); the injected `WebSocketConnection` does the I/O.
//!
//! The session-id derivation reuses [`hermit_common::extensions::get_md5`] (the
//! C# `key.GetMD5().ToString("N")`), not a re-implemented hash.
//!
//! ## Unit 9 — deferred-subsystem stubs + scheduled-task registry
//!
//! The deferred subsystems and the scheduler-less task registry:
//! - [`channel_manager`] — [`HermitChannelManager`]
//!   ([`hermit_traits::stubs::ChannelManager`]), empty channel results;
//! - [`sync_play_manager`] — [`HermitSyncPlayManager`]
//!   ([`hermit_traits::stubs::SyncPlayManager`]), a no-op group coordinator;
//! - [`plugin_manager`] — [`HermitPluginManager`]
//!   ([`hermit_traits::plugins::PluginManager`]), the Tier-1 registry-backed
//!   manager over compiled-in plugins (empty until the composition root registers
//!   any);
//! - [`lyric_manager`] — [`HermitLyricManager`]
//!   ([`hermit_traits::stubs::LyricManager`]), empty lyrics;
//! - [`subtitle_manager`] — [`HermitSubtitleManager`]
//!   ([`hermit_traits::subtitles::SubtitleManager`]), the portable
//!   stored-external-subtitle slice (delete a stream + its sidecar); the
//!   provider fan-out (search/download/upload) is a documented deferral;
//! - [`scheduled_tasks`] — [`HermitTaskManager`] + the local [`ScheduledTask`]
//!   trait, a minimal register/list/run-now registry over the `hermit-model`
//!   task DTOs. **No cron loop**: a task only runs on an explicit
//!   [`run_now`](HermitTaskManager::run_now); the `ITaskTrigger` timers, the
//!   background queue and the on-disk trigger/result persistence are deferred to
//!   a future scheduler wave. `FullSystemBackup`/`BackupService` is deferred
//!   entirely.

pub mod activity_manager;
pub mod api_key_manager;
pub mod app_paths;
pub mod application_host;
pub mod auth_providers;
pub mod authorization_context;
pub mod channel_manager;
pub mod chapter_manager;
pub mod chapter_repository;
pub mod client_event_logger;
pub mod collection_manager;
pub mod configuration_manager;
pub mod db_error;
pub mod device_manager;
pub mod display_preferences_manager;
pub mod dto_service;
pub mod event_manager;
pub mod external_data_manager;
pub mod file_system;
pub mod item_count_service;
pub mod item_persistence_service;
pub mod item_repository;
pub mod item_type_lookup;
pub mod keyframe_repository;
pub mod kinds;
pub mod library_manager;
pub mod library_monitor;
pub mod library_scan;
pub mod linked_children_service;
pub mod localization_manager;
pub mod lyric_manager;
pub mod media_attachment_repository;
pub mod media_segment_manager;
pub mod media_source_manager;
pub mod media_stream_repository;
pub mod music_manager;
pub mod next_up_service;
pub mod path_manager;
pub mod people_repository;
pub mod plugin_manager;
pub mod quick_connect_manager;
pub mod resolvers;
pub mod scheduled_tasks;
pub mod search_manager;
pub mod session_bus;
pub mod session_manager;
pub mod session_websocket_listener;
pub mod similar_items_manager;
pub mod subtitle_manager;
pub mod sync_play_manager;
pub mod system_manager;
pub mod text_util;
pub mod translate_query;
pub mod trickplay_manager;
pub mod tv_series_manager;
pub mod user_data_manager;
pub mod user_entity_ext;
pub mod user_manager;
pub mod user_view_manager;
pub mod virtual_folder_manager;

#[cfg(test)]
mod test_support;

pub use activity_manager::HermitActivityManager;
pub use api_key_manager::HermitApiKeyManager;
pub use app_paths::HermitServerApplicationPaths;
pub use application_host::{HermitServerApplicationHost, HostNetworkInfo};
pub use auth_providers::{DefaultAuthenticationProvider, InvalidAuthProvider};
pub use authorization_context::{HermitAuthService, HermitAuthorizationContext};
pub use channel_manager::HermitChannelManager;
pub use chapter_manager::HermitChapterManager;
pub use chapter_repository::HermitChapterRepository;
pub use client_event_logger::HermitClientEventLogger;
pub use collection_manager::{HermitCollectionManager, HermitPlaylistManager};
pub use configuration_manager::{HermitServerConfigurationManager, default_server_configuration};
pub use device_manager::HermitDeviceManager;
pub use display_preferences_manager::HermitDisplayPreferencesManager;
pub use dto_service::HermitDtoService;
pub use event_manager::{EventConsumer, HermitEventManager};
pub use external_data_manager::HermitExternalDataManager;
pub use file_system::HermitFileSystem;
pub use item_count_service::HermitItemCountService;
pub use item_persistence_service::HermitItemPersistenceService;
pub use item_repository::HermitItemRepository;
pub use item_type_lookup::ItemTypeLookup;
pub use keyframe_repository::HermitKeyframeRepository;
pub use library_manager::HermitLibraryManager;
pub use library_monitor::{HermitLibraryMonitor, LibraryScanTrigger, NoopFileSystemWatcher};
pub use library_scan::LibraryScanner;
pub use linked_children_service::HermitLinkedChildrenService;
pub use localization_manager::LocalizationManager;
pub use lyric_manager::HermitLyricManager;
pub use media_attachment_repository::HermitMediaAttachmentRepository;
pub use media_segment_manager::HermitMediaSegmentManager;
pub use media_source_manager::HermitMediaSourceManager;
pub use media_stream_repository::HermitMediaStreamRepository;
pub use music_manager::HermitMusicManager;
pub use next_up_service::HermitNextUpService;
pub use path_manager::HermitPathManager;
pub use people_repository::HermitPeopleRepository;
pub use plugin_manager::{HermitPluginManager, RegisteredPlugin};
pub use quick_connect_manager::HermitQuickConnect;
pub use scheduled_tasks::{HermitTaskManager, RefreshLibraryTask, ScheduledTask};
pub use search_manager::HermitSearchManager;
pub use session_bus::HermitSessionMessageBus;
pub use session_manager::HermitSessionManager;
pub use session_websocket_listener::{HermitSessionWebSocketListener, HermitWebSocketManager};
pub use similar_items_manager::HermitSimilarItemsManager;
pub use subtitle_manager::HermitSubtitleManager;
pub use sync_play_manager::HermitSyncPlayManager;
pub use system_manager::HermitSystemManager;
pub use trickplay_manager::HermitTrickplayManager;
pub use tv_series_manager::HermitTvSeriesManager;
pub use user_data_manager::HermitUserDataManager;
pub use user_manager::HermitUserManager;
pub use user_view_manager::HermitUserViewManager;
pub use virtual_folder_manager::HermitVirtualFolderManager;
