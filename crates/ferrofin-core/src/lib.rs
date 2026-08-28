//! Core manager implementations for Ferrofin — the workhorse.
//!
//! Port of `Emby.Server.Implementations` + `Jellyfin.Server.Implementations`.
//! Provides the concrete implementations of the `ferrofin-traits` manager traits:
//! item repository (the `InternalItemsQuery` → SQL builder over `ferrofin-db`),
//! library scanning, user/auth, sessions, DTO assembly (entity → `BaseItemDto`),
//! media sources, config/system/devices, plus the `BaseItem` domain behavior as
//! free functions over `BaseItemKind`.
//!
//! Sibling managers (MediaEncoder, ImageProcessor, ProviderManager, …) are taken
//! as `Arc<dyn Trait>` and injected at the composition root (`ferrofin-server`,
//! Wave 8) — this crate depends only on `ferrofin-traits` for them, not on the
//! impl crates. Peripheral + deferred-subsystem managers are minimal/stub.
//!
//! ## Unit 1 — item repository + query translation (First-Light core)
//!
//! This unit lands the foundation:
//! - [`kinds`] — the `BaseItem`/`Folder`/`Video` OOP behavior as free functions
//!   over [`ferrofin_model::data::BaseItemKind`];
//! - [`item_type_lookup`] — the static kind → stored-type-name tables
//!   ([`ItemTypeLookup`]);
//! - [`translate_query`] — the [`ferrofin_traits::options::InternalItemsQuery`] →
//!   SQL builder (a `sqlx::QueryBuilder` translator over `ferrofin-db`);
//! - [`item_repository`] — [`FerrofinItemRepository`] ([`ferrofin_traits::persistence::ItemRepository`]);
//! - [`item_count_service`] — [`FerrofinItemCountService`] ([`ferrofin_traits::persistence::ItemCountService`]);
//! - [`item_persistence_service`] — [`FerrofinItemPersistenceService`]
//!   ([`ferrofin_traits::persistence::ItemPersistenceService`]).
//!
//! ## Unit 2 — per-item sub-repositories
//!
//! Row-level CRUD for an item's child collections plus the two linked-item
//! services, all over `ferrofin-db` and reusing the unit-1 `kinds`/type-lookup
//! helpers and (for next-up) the stored `Type` names:
//! - [`chapter_repository`] — [`FerrofinChapterRepository`]
//!   ([`ferrofin_traits::persistence::ChapterRepository`]);
//! - [`media_stream_repository`] — [`FerrofinMediaStreamRepository`]
//!   ([`ferrofin_traits::persistence::MediaStreamRepository`]);
//! - [`media_attachment_repository`] — [`FerrofinMediaAttachmentRepository`]
//!   ([`ferrofin_traits::persistence::MediaAttachmentRepository`]);
//! - [`people_repository`] — [`FerrofinPeopleRepository`]
//!   ([`ferrofin_traits::persistence::PeopleRepository`]);
//! - [`keyframe_repository`] — [`FerrofinKeyframeRepository`]
//!   ([`ferrofin_traits::persistence::KeyframeRepository`]);
//! - [`linked_children_service`] — [`FerrofinLinkedChildrenService`]
//!   ([`ferrofin_traits::persistence::LinkedChildrenService`]);
//! - [`next_up_service`] — [`FerrofinNextUpService`]
//!   ([`ferrofin_traits::persistence::NextUpService`]), whose
//!   [`NextUpEpisodeBatchResult`](ferrofin_traits::persistence::NextUpEpisodeBatchResult)
//!   output is consumed by `TvSeriesManager` in a later unit.
//!
//! The shared [`db_error`] module holds the single `sqlx` → `ServiceError`
//! mapping and the `MediaStreamType` discriminant helper reused across units.
//!
//! ## Unit 5 — library manager, media sources, views, search, music
//!
//! The library-orchestration seam, all delegating to the unit 1–2 persistence
//! repositories (taken as `Arc<dyn _>`) rather than touching the pool directly:
//! - [`library_manager`] — [`FerrofinLibraryManager`]
//!   ([`ferrofin_traits::library::LibraryManager`]);
//! - [`media_source_manager`] — [`FerrofinMediaSourceManager`]
//!   ([`ferrofin_traits::library::MediaSourceManager`]), holding in-memory
//!   live-stream state and injecting `Arc<dyn MediaEncoder>` / `Arc<dyn
//!   ProviderManager>` for probing;
//! - [`user_view_manager`] — [`FerrofinUserViewManager`]
//!   ([`ferrofin_traits::library::UserViewManager`]);
//! - [`search_manager`] — [`FerrofinSearchManager`]
//!   ([`ferrofin_traits::library::SearchManager`]);
//! - [`music_manager`] — [`FerrofinMusicManager`]
//!   ([`ferrofin_traits::library::MusicManager`]);
//! - [`similar_items_manager`] — [`FerrofinSimilarItemsManager`]
//!   ([`ferrofin_traits::library::SimilarItemsManager`]);
//! - [`library_monitor`] — [`FerrofinLibraryMonitor`]
//!   ([`ferrofin_traits::library::LibraryMonitor`]), watching library filesystems
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
//! - [`dto_service`] — [`FerrofinDtoService`]
//!   ([`ferrofin_traits::dto::DtoService`]), the entity → [`BaseItemDto`](ferrofin_model::dto::BaseItemDto)
//!   assembly. It reads via the injected [`LibraryManager`](ferrofin_traits::library::LibraryManager) /
//!   [`UserDataManager`](ferrofin_traits::library::UserDataManager) /
//!   [`ItemCountService`](ferrofin_traits::persistence::ItemCountService),
//!   resolves images through an injected `Arc<dyn ImageProcessor>`, honors the
//!   [`DtoOptions`](ferrofin_traits::options::DtoOptions) field/image toggles, and
//!   reuses the [`kinds`] helpers to branch on an item's kind (the C#
//!   subclass type-tests). LiveTV program/channel enrichment and active-recording
//!   rewrites are deferred (their sibling seams are not injected into this unit).
//!
//! ## Unit 7 — session manager + WebSocket wiring
//!
//! The session/eventing seam:
//! - [`session_manager`] — [`FerrofinSessionManager`]
//!   ([`ferrofin_traits::session::SessionManager`]), the largest impl. Owns the
//!   in-memory `SessionInfo` table (the C# `_activeConnections`), reports
//!   playback through the injected [`UserDataManager`](ferrofin_traits::library::UserDataManager),
//!   and broadcasts server → client messages as pre-serialized JSON to each
//!   session's attached WebSocket connections. Sibling managers (user/device/
//!   user-data/library/DTO) plus the [`EventManager`](ferrofin_traits::events::EventManager)
//!   are injected; idle timers, instant-mix, and live-stream reference-counting
//!   are documented deferrals.
//! - [`event_manager`] — [`FerrofinEventManager`]
//!   ([`ferrofin_traits::events::EventManager`]), the in-process publish seam: a
//!   name-keyed [`EventConsumer`] registry replacing the C# DI-scoped
//!   `IEventConsumer<T>` lookup; publication logs-and-continues past a failing
//!   consumer.
//! - [`client_event_logger`] — [`FerrofinClientEventLogger`]
//!   ([`ferrofin_traits::events::ClientEventLogger`]), persisting client-uploaded
//!   diagnostic documents under the injected paths' log directory with a
//!   traversal-safe file name.
//! - [`session_websocket_listener`] — [`FerrofinSessionWebSocketListener`]
//!   ([`ferrofin_traits::net::WebSocketListener`]) resolving+attaching a
//!   connection to its session on connect, and [`FerrofinWebSocketManager`]
//!   ([`ferrofin_traits::net::WebSocketManager`]) validating the upgrade request.
//!   The actual HTTP → WS upgrade and per-connection receive loop belong to the
//!   HTTP layer (Wave 7); the injected `WebSocketConnection` does the I/O.
//!
//! The session-id derivation reuses [`ferrofin_common::extensions::get_md5`] (the
//! C# `key.GetMD5().ToString("N")`), not a re-implemented hash.
//!
//! ## Unit 9 — deferred-subsystem stubs + scheduled-task registry
//!
//! The deferred subsystems and the scheduler-less task registry:
//! - [`channel_manager`] — [`FerrofinChannelManager`]
//!   ([`ferrofin_traits::stubs::ChannelManager`]), empty channel results;
//! - [`sync_play_manager`] — [`FerrofinSyncPlayManager`]
//!   ([`ferrofin_traits::stubs::SyncPlayManager`]), a no-op group coordinator;
//! - [`plugin_manager`] — [`FerrofinPluginManager`]
//!   ([`ferrofin_traits::plugins::PluginManager`]), the Tier-1 registry-backed
//!   manager over compiled-in plugins (empty until the composition root registers
//!   any);
//! - [`lyric_manager`] — [`FerrofinLyricManager`]
//!   ([`ferrofin_traits::stubs::LyricManager`]), empty lyrics;
//! - [`subtitle_manager`] — [`FerrofinSubtitleManager`]
//!   ([`ferrofin_traits::subtitles::SubtitleManager`]), the portable
//!   stored-external-subtitle slice (delete a stream + its sidecar); the
//!   provider fan-out (search/download/upload) is a documented deferral;
//! - [`scheduled_tasks`] — [`FerrofinTaskManager`] + the local [`ScheduledTask`]
//!   trait: the task registry over the `ferrofin-model` task DTOs, plus the
//!   trigger scheduler ([`start_scheduler`](FerrofinTaskManager::start_scheduler)
//!   fires daily/weekly/interval/startup triggers), live progress reporting,
//!   abortable queued runs, and on-disk trigger-override persistence.
//!   `FullSystemBackup`/`BackupService` is deferred entirely.

pub mod access_schedule_repository;
pub mod activity_manager;
pub mod api_key_manager;
pub mod app_paths;
pub mod application_host;
pub mod auth_cache;
pub mod auth_providers;
pub mod authorization_context;
pub mod channel_manager;
pub mod chapter_manager;
pub mod chapter_repository;
pub mod client_event_logger;
pub mod collection_manager;
pub mod config_import;
pub mod configuration_manager;
pub mod db_error;
pub mod device_manager;
mod device_repository;
pub mod display_preferences_manager;
pub mod dto_service;
pub mod dynamic_images;
pub mod event_manager;
pub mod external_data_manager;
pub mod file_system;
pub mod item_count_service;
pub mod item_data;
pub mod item_persistence_service;
pub mod item_repository;
pub mod item_type_lookup;
pub mod keyframe_repository;
pub mod kinds;
pub mod library_manager;
pub mod library_monitor;
pub mod library_scan;
pub mod linked_children_service;
pub mod live_tv_import;
pub mod localization_manager;
pub mod lyric_manager;
pub mod media_attachment_repository;
pub mod media_info_resolver;
pub mod media_segment_manager;
pub mod media_source_manager;
pub mod media_stream_repository;
pub mod music_manager;
pub mod next_up_service;
pub mod notify_watcher;
pub mod path_manager;
pub mod people_repository;
pub mod playback_metrics;
pub mod plugin_manager;
pub mod quick_connect_manager;
pub mod resolvers;
pub mod scheduled_tasks;
pub mod search_manager;
pub mod session_bus;
pub mod session_manager;
pub mod session_websocket_listener;
pub mod similar_items_manager;
mod similar_items_repository;
pub mod subtitle_manager;
pub mod sync_play_manager;
pub mod system_manager;
pub mod text_util;
pub mod translate_query;
pub mod trickplay_manager;
pub mod tv_series_manager;
pub mod user_data_keys;
pub mod user_data_manager;
pub mod user_entity_ext;
pub mod user_manager;
pub mod user_root_folder;
pub mod user_view_manager;
pub mod virtual_folder_manager;
pub mod years;

#[cfg(test)]
mod test_support;

pub use activity_manager::FerrofinActivityManager;
pub use api_key_manager::FerrofinApiKeyManager;
pub use app_paths::FerrofinServerApplicationPaths;
pub use application_host::{FerrofinServerApplicationHost, HostNetworkInfo};
pub use auth_providers::{DefaultAuthenticationProvider, InvalidAuthProvider};
pub use authorization_context::{FerrofinAuthService, FerrofinAuthorizationContext};
pub use channel_manager::FerrofinChannelManager;
pub use chapter_manager::FerrofinChapterManager;
pub use chapter_repository::FerrofinChapterRepository;
pub use client_event_logger::FerrofinClientEventLogger;
pub use collection_manager::{FerrofinCollectionManager, FerrofinPlaylistManager};
pub use configuration_manager::{FerrofinServerConfigurationManager, default_server_configuration};
pub use device_manager::FerrofinDeviceManager;
pub use display_preferences_manager::FerrofinDisplayPreferencesManager;
pub use dto_service::FerrofinDtoService;
pub use event_manager::{EventConsumer, FerrofinEventManager};
pub use external_data_manager::FerrofinExternalDataManager;
pub use file_system::FerrofinFileSystem;
pub use item_count_service::FerrofinItemCountService;
pub use item_persistence_service::FerrofinItemPersistenceService;
pub use item_repository::FerrofinItemRepository;
pub use item_type_lookup::ItemTypeLookup;
pub use keyframe_repository::FerrofinKeyframeRepository;
pub use library_manager::FerrofinLibraryManager;
pub use library_monitor::{
    FerrofinLibraryMonitor, LibraryScanTrigger, NoopFileSystemWatcher, WatchRootsSource,
};
pub use library_scan::LibraryScanner;
pub use linked_children_service::FerrofinLinkedChildrenService;
pub use localization_manager::LocalizationManager;
pub use lyric_manager::FerrofinLyricManager;
pub use media_attachment_repository::FerrofinMediaAttachmentRepository;
pub use media_info_resolver::{ExternalMediaTarget, ExternalStreamResolvers, MediaInfoResolver};
pub use media_segment_manager::FerrofinMediaSegmentManager;
pub use media_source_manager::FerrofinMediaSourceManager;
pub use media_stream_repository::FerrofinMediaStreamRepository;
pub use music_manager::FerrofinMusicManager;
pub use next_up_service::FerrofinNextUpService;
pub use notify_watcher::NotifyFileSystemWatcher;
pub use path_manager::FerrofinPathManager;
pub use people_repository::FerrofinPeopleRepository;
pub use playback_metrics::FerrofinPlaybackMetrics;
pub use plugin_manager::{
    FerrofinPluginManager, PluginConfigPage, RegisteredPlugin, merge_plugin_registrations,
};
pub use quick_connect_manager::FerrofinQuickConnect;
pub use scheduled_tasks::{FerrofinTaskManager, RefreshLibraryTask, ScheduledTask, TaskProgress};
pub use search_manager::FerrofinSearchManager;
pub use session_bus::FerrofinSessionMessageBus;
pub use session_manager::FerrofinSessionManager;
pub use session_websocket_listener::{FerrofinSessionWebSocketListener, FerrofinWebSocketManager};
pub use similar_items_manager::FerrofinSimilarItemsManager;
pub use subtitle_manager::FerrofinSubtitleManager;
pub use sync_play_manager::FerrofinSyncPlayManager;
pub use system_manager::FerrofinSystemManager;
pub use trickplay_manager::FerrofinTrickplayManager;
pub use tv_series_manager::FerrofinTvSeriesManager;
pub use user_data_manager::FerrofinUserDataManager;
pub use user_manager::FerrofinUserManager;
pub use user_root_folder::UserRootFolderStore;
pub use user_view_manager::FerrofinUserViewManager;
pub use virtual_folder_manager::FerrofinVirtualFolderManager;
pub use years::YearStore;
