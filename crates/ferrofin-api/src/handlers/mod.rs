//! Real ported handlers for the First-Light routes.
//!
//! Each submodule mirrors one `Jellyfin.Api` controller and holds axum handlers
//! that call the [`AppState`](crate::state::AppState) manager traits, project
//! results through [`DtoService`](ferrofin_traits::dto::DtoService), and return
//! the wire DTOs from `ferrofin-model`. These are the routes with *real* behaviour
//! (the rest of the contract stays on the shared `not_implemented` `501` stub);
//! [`register`] mounts them over their stub entries.
//!
//! Handlers behind Jellyfin's `[Authorize]` policy take the
//! [`RequireAuth`](crate::auth::RequireAuth) extractor (a missing/invalid token
//! becomes `401`); public routes read the (possibly anonymous)
//! [`AuthorizationInfo`](ferrofin_traits::options::AuthorizationInfo) extension set
//! by the auth-context middleware.

use axum::Router;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Queues a default high-priority metadata refresh for an item.
///
/// Ports the `_providerManager.QueueRefresh(item.Id, new MetadataRefreshOptions(…),
/// RefreshPriority.High)` call the subtitle/lyric upload+download handlers make
/// after mutating an item's sidecar files, so the freshly added stream is picked
/// up. Uses [`MetadataRefreshOptions::default`] (the C# constructor with only a
/// directory service is a no-op-mode refresh).
pub(crate) async fn queue_high_priority_refresh(
    state: &AppState,
    item_id: Uuid,
) -> Result<(), ApiError> {
    use ferrofin_traits::providers::{MetadataRefreshOptions, RefreshPriority};
    state
        .providers
        .queue_refresh(
            item_id,
            &MetadataRefreshOptions::default(),
            RefreshPriority::High,
        )
        .await?;
    Ok(())
}

pub mod activity_log;
pub mod api_key;
pub mod artists;
pub mod audio;
pub mod backup;
pub mod branding;
pub mod by_name;
pub mod channels;
pub mod client_log;
pub mod collection;
pub mod config;
pub mod dashboard;
pub mod devices;
pub mod display_preferences;
pub mod environment;
pub mod filter;
pub mod genres;
pub mod hls;
pub(crate) mod image_upload;
pub mod images;
pub mod instant_mix;
pub mod intro_skipper;
pub mod item_lookup;
pub mod item_update;
pub mod items;
pub mod library;
pub mod library_structure;
pub mod live_tv;
pub mod localization;
pub mod lyrics;
pub mod media_info;
pub mod media_segments;
pub mod merge_versions;
pub mod movies;
pub mod music_genres;
pub mod persons;
pub mod playlists;
pub mod playstate;
pub mod plugins;
pub(crate) mod query_parse;
pub mod quick_connect;
pub mod remote_images;
pub mod scheduled_tasks;
pub mod search;
pub mod session;
pub(crate) mod session_ctx;
pub mod similar;
pub mod startup;
pub(crate) mod streaming;
pub mod studios;
pub mod subtitles;
pub mod suggestions;
pub mod sync_play;
pub mod system;
pub mod time_sync;
pub mod tmdb;
pub mod trailers;
pub mod trickplay;
pub mod tv_shows;
pub mod user_library;
pub mod user_views;
pub mod users;
pub mod videos;
pub mod websocket;
pub mod years;

/// The `(method, axum_path)` pairs served by a real handler in this unit.
///
/// [`create_router`](crate::router::create_router) skips the shared `501` stub
/// for each of these so the real handler is the sole route for that
/// `(method, path)` (axum panics on two handlers for the same method+path). The
/// paths are the axum-normalized forms (they already use axum's `{param}`
/// capture syntax and match the vendored-contract normalization).
pub const REAL_ROUTES: &[(&str, &str)] = &[
    ("get", "/System/Info"),
    ("get", "/System/Info/Public"),
    ("post", "/Users/AuthenticateByName"),
    ("get", "/Users/Me"),
    ("get", "/UserViews"),
    ("get", "/Items"),
    ("delete", "/Items"),
    ("get", "/Items/Counts"),
    ("get", "/Items/{itemId}"),
    ("post", "/Items/{itemId}"),
    ("delete", "/Items/{itemId}"),
    ("get", "/Items/{itemId}/Ancestors"),
    ("post", "/Items/{itemId}/ContentType"),
    ("post", "/Items/{itemId}/Refresh"),
    ("get", "/Items/{itemId}/PlaybackInfo"),
    ("post", "/Items/{itemId}/PlaybackInfo"),
    ("get", "/Videos/{itemId}/stream"),
    ("head", "/Videos/{itemId}/stream"),
    ("get", "/Items/{itemId}/Images/{imageType}"),
    ("head", "/Items/{itemId}/Images/{imageType}"),
    // Batch 1 — by-name browse foundation.
    ("get", "/Genres"),
    ("get", "/Genres/{genreName}"),
    ("get", "/MusicGenres"),
    ("get", "/MusicGenres/{genreName}"),
    ("get", "/Studios"),
    ("get", "/Studios/{name}"),
    ("get", "/Persons"),
    ("get", "/Persons/{name}"),
    ("get", "/Artists"),
    ("get", "/Artists/AlbumArtists"),
    // The vendored `/Artists/{name}` normalizes to `{itemId}` — the position's
    // first-seen param name across the table (see `routes::normalize_contract_path`).
    ("get", "/Artists/{itemId}"),
    ("get", "/Years"),
    ("get", "/Years/{year}"),
    // Batch 3 — filters, suggestions, instant-mix, similar/recommendations.
    ("get", "/Items/Filters"),
    ("get", "/Items/Filters2"),
    ("get", "/Items/Suggestions"),
    ("get", "/Songs/{itemId}/InstantMix"),
    ("get", "/Albums/{itemId}/InstantMix"),
    ("get", "/Playlists/{itemId}/InstantMix"),
    ("get", "/Artists/InstantMix"),
    ("get", "/Artists/{itemId}/InstantMix"),
    ("get", "/Items/{itemId}/InstantMix"),
    ("get", "/MusicGenres/InstantMix"),
    // The vendored `/MusicGenres/{name}/InstantMix` normalizes to `{genreName}`
    // (the position's first-seen param name, from Batch 1's `/MusicGenres/{genreName}`).
    ("get", "/MusicGenres/{genreName}/InstantMix"),
    ("get", "/Movies/Recommendations"),
    ("get", "/Trailers"),
    ("get", "/Search/Hints"),
    // Batch 4 — user library + user items + play-state flags.
    ("post", "/UserFavoriteItems/{itemId}"),
    ("delete", "/UserFavoriteItems/{itemId}"),
    ("post", "/UserItems/{itemId}/Rating"),
    ("delete", "/UserItems/{itemId}/Rating"),
    ("get", "/UserItems/{itemId}/UserData"),
    ("post", "/UserItems/{itemId}/UserData"),
    ("get", "/UserItems/Resume"),
    ("get", "/Items/Root"),
    ("get", "/Items/Latest"),
    ("get", "/Items/{itemId}/LocalTrailers"),
    ("get", "/Items/{itemId}/SpecialFeatures"),
    ("get", "/Items/{itemId}/Intros"),
    ("get", "/Items/{itemId}/CriticReviews"),
    // Batch 5 — Playstate + Session playback reporting.
    ("post", "/UserPlayedItems/{itemId}"),
    ("delete", "/UserPlayedItems/{itemId}"),
    ("post", "/Sessions/Playing"),
    ("post", "/Sessions/Playing/Progress"),
    ("post", "/Sessions/Playing/Ping"),
    ("post", "/Sessions/Playing/Stopped"),
    ("post", "/PlayingItems/{itemId}"),
    ("delete", "/PlayingItems/{itemId}"),
    ("post", "/PlayingItems/{itemId}/Progress"),
    ("get", "/Sessions"),
    ("post", "/Sessions/{sessionId}/Viewing"),
    ("post", "/Sessions/{sessionId}/Playing"),
    ("post", "/Sessions/{sessionId}/Playing/{command}"),
    ("post", "/Sessions/{sessionId}/System/{command}"),
    ("post", "/Sessions/{sessionId}/Command/{command}"),
    ("post", "/Sessions/{sessionId}/Command"),
    ("post", "/Sessions/{sessionId}/Message"),
    ("post", "/Sessions/{sessionId}/User/{userId}"),
    ("delete", "/Sessions/{sessionId}/User/{userId}"),
    ("post", "/Sessions/Capabilities"),
    ("post", "/Sessions/Capabilities/Full"),
    ("post", "/Sessions/Viewing"),
    ("post", "/Sessions/Logout"),
    ("get", "/Auth/Providers"),
    ("get", "/Auth/PasswordResetProviders"),
    // Batch 6 — Users admin + Startup + QuickConnect.
    ("get", "/Users"),
    ("post", "/Users"),
    ("get", "/Users/Public"),
    ("get", "/Users/{userId}"),
    ("delete", "/Users/{userId}"),
    ("post", "/Users/New"),
    ("post", "/Users/{userId}/Policy"),
    ("post", "/Users/Configuration"),
    ("post", "/Users/Password"),
    ("post", "/Users/AuthenticateWithQuickConnect"),
    ("post", "/Users/ForgotPassword"),
    ("post", "/Users/ForgotPassword/Pin"),
    ("post", "/Startup/Complete"),
    ("get", "/Startup/Configuration"),
    ("post", "/Startup/Configuration"),
    ("post", "/Startup/RemoteAccess"),
    ("get", "/Startup/User"),
    ("post", "/Startup/User"),
    ("get", "/Startup/FirstUser"),
    ("get", "/QuickConnect/Enabled"),
    ("post", "/QuickConnect/Initiate"),
    ("get", "/QuickConnect/Connect"),
    ("post", "/QuickConnect/Authorize"),
    // Batch 7 — Playlists + Collections.
    ("post", "/Playlists"),
    // The vendored `/Playlists/{playlistId}` normalizes to `{itemId}` — the
    // position's first-seen param name (from Batch 3's `/Playlists/{itemId}/InstantMix`).
    ("get", "/Playlists/{itemId}"),
    ("post", "/Playlists/{itemId}"),
    ("get", "/Playlists/{itemId}/Items"),
    ("post", "/Playlists/{itemId}/Items"),
    ("delete", "/Playlists/{itemId}/Items"),
    // The move route's `{playlistId}` + `{itemId}` both normalize to `{itemId}`.
    ("post", "/Playlists/{itemId}/Items/{itemId}/Move/{newIndex}"),
    ("get", "/Playlists/{itemId}/Users"),
    ("get", "/Playlists/{itemId}/Users/{userId}"),
    ("post", "/Playlists/{itemId}/Users/{userId}"),
    ("delete", "/Playlists/{itemId}/Users/{userId}"),
    ("post", "/Collections"),
    ("post", "/Collections/{collectionId}/Items"),
    ("delete", "/Collections/{collectionId}/Items"),
    // Batch 8 — TV shows: next-up, upcoming, seasons/episodes, similar.
    ("get", "/Shows/NextUp"),
    ("get", "/Shows/Upcoming"),
    // The vendored `/Shows/{seriesId}/…` paths normalize to `{itemId}` — the
    // position's first-seen param name (from `/Shows/{itemId}/Similar`).
    ("get", "/Shows/{itemId}/Similar"),
    ("get", "/Shows/{itemId}/Episodes"),
    ("get", "/Shows/{itemId}/Seasons"),
    // Batch 9 — image variants (item / by-name / user / remote), read side.
    // Item image infos + serving (the two base item-image entries live above in
    // First-Light; these add the indexed, long-parametrized, and infos routes).
    ("get", "/Items/{itemId}/Images"),
    ("get", "/Items/{itemId}/Images/{imageType}/{imageIndex}"),
    ("head", "/Items/{itemId}/Images/{imageType}/{imageIndex}"),
    (
        "get",
        "/Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}",
    ),
    (
        "head",
        "/Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}",
    ),
    // By-name images. The `{name}` segment normalizes per the vendored table's
    // first-seen name at that position: Genres/MusicGenres → `{genreName}`,
    // Studios/Persons → `{name}`, Artists → `{itemId}`.
    ("get", "/Genres/{genreName}/Images/{imageType}"),
    ("head", "/Genres/{genreName}/Images/{imageType}"),
    ("get", "/Genres/{genreName}/Images/{imageType}/{imageIndex}"),
    (
        "head",
        "/Genres/{genreName}/Images/{imageType}/{imageIndex}",
    ),
    ("get", "/MusicGenres/{genreName}/Images/{imageType}"),
    ("head", "/MusicGenres/{genreName}/Images/{imageType}"),
    (
        "get",
        "/MusicGenres/{genreName}/Images/{imageType}/{imageIndex}",
    ),
    (
        "head",
        "/MusicGenres/{genreName}/Images/{imageType}/{imageIndex}",
    ),
    ("get", "/Studios/{name}/Images/{imageType}"),
    ("head", "/Studios/{name}/Images/{imageType}"),
    ("get", "/Studios/{name}/Images/{imageType}/{imageIndex}"),
    ("head", "/Studios/{name}/Images/{imageType}/{imageIndex}"),
    ("get", "/Persons/{name}/Images/{imageType}"),
    ("head", "/Persons/{name}/Images/{imageType}"),
    ("get", "/Persons/{name}/Images/{imageType}/{imageIndex}"),
    ("head", "/Persons/{name}/Images/{imageType}/{imageIndex}"),
    ("get", "/Artists/{itemId}/Images/{imageType}/{imageIndex}"),
    ("head", "/Artists/{itemId}/Images/{imageType}/{imageIndex}"),
    // User profile image (serve + clear).
    ("get", "/UserImage"),
    ("head", "/UserImage"),
    ("delete", "/UserImage"),
    // Remote (provider) images.
    ("get", "/Items/{itemId}/RemoteImages"),
    ("get", "/Items/{itemId}/RemoteImages/Providers"),
    ("post", "/Items/{itemId}/RemoteImages/Download"),
    // Batch 10 — Videos + Audio direct streaming, Universal, MediaInfo.
    // Direct-play (static) file serving; transcoding/HLS stay on the 501 stub.
    // The vendored `stream.{container}` segment normalizes to `{container}` — a
    // single param capture at that position (see `routes::normalize_contract_path`).
    ("get", "/Videos/{itemId}/{container}"),
    ("head", "/Videos/{itemId}/{container}"),
    ("get", "/Audio/{itemId}/stream"),
    ("head", "/Audio/{itemId}/stream"),
    ("get", "/Audio/{itemId}/{container}"),
    ("head", "/Audio/{itemId}/{container}"),
    ("get", "/Audio/{itemId}/universal"),
    ("head", "/Audio/{itemId}/universal"),
    // Media download (LibraryController.GetDownload).
    ("get", "/Items/{itemId}/Download"),
    // Version-group management + additional parts.
    ("get", "/Videos/{itemId}/AdditionalParts"),
    ("post", "/Videos/MergeVersions"),
    ("delete", "/Videos/{itemId}/AlternateSources"),
    // HLS / dynamic-transcode streaming (DynamicHlsController +
    // HlsSegmentController + VideoAttachmentsController). Playlists, dynamic
    // segments, the legacy segment/playlist serve, stop-encoding, and attachment
    // serve — wired to the real transcode runtime + ferrofin-hls generator via the
    // `HlsStreamManager` seam. Normalized axum paths (the `.container`/`.m3u8`
    // literals are dropped from multi-param segments; `stream.m3u8` /
    // `stream.{aac,mp3}` keep their literal trailing segment). The
    // `stream.{container}` transcode branch reuses the already-real
    // `/Videos|Audio/{itemId}/{container}` + `/Audio/{itemId}/universal` routes.
    ("get", "/Videos/{itemId}/master.m3u8"),
    ("head", "/Videos/{itemId}/master.m3u8"),
    ("get", "/Videos/{itemId}/main.m3u8"),
    ("get", "/Videos/{itemId}/live.m3u8"),
    ("get", "/Videos/{itemId}/hls1/{playlistId}/{segmentId}"),
    ("get", "/Videos/{itemId}/hls/{playlistId}/{segmentId}"),
    ("get", "/Videos/{itemId}/hls/{playlistId}/stream.m3u8"),
    ("get", "/Audio/{itemId}/master.m3u8"),
    ("head", "/Audio/{itemId}/master.m3u8"),
    ("get", "/Audio/{itemId}/main.m3u8"),
    ("get", "/Audio/{itemId}/hls1/{playlistId}/{segmentId}"),
    ("get", "/Audio/{itemId}/hls/{segmentId}/stream.aac"),
    ("get", "/Audio/{itemId}/hls/{segmentId}/stream.mp3"),
    ("delete", "/Videos/ActiveEncodings"),
    // Attachment serve normalizes `{videoId}/{mediaSourceId}` → `{itemId}/{container}`.
    ("get", "/Videos/{itemId}/{container}/Attachments/{index}"),
    // Live streams + bitrate test (MediaInfoController).
    ("post", "/LiveStreams/Open"),
    ("post", "/LiveStreams/Close"),
    ("get", "/Playback/BitrateTest"),
    // Backup: list/inspect/create/restore DB + config archives.
    ("get", "/Backup"),
    ("get", "/Backup/Manifest"),
    ("post", "/Backup/Create"),
    ("post", "/Backup/Restore"),
    // Channels (empty — no channel providers exist without plugins).
    ("get", "/Channels"),
    ("get", "/Channels/Features"),
    ("get", "/Channels/{channelId}/Features"),
    ("get", "/Channels/{channelId}/Items"),
    ("get", "/Channels/Items/Latest"),
    // Batch 11 — Subtitles + Lyrics + MediaSegments + Trickplay.
    // Media segments (read); the plugin `SegmentEditor` `/MediaSegmentsApi/*`
    // routes stay on the 501 stub (dynamic plugin host is deferred).
    ("get", "/MediaSegments/{itemId}"),
    // Trickplay playlist + tile. The `{index}.jpg` segment normalizes to a bare
    // `{index}` capture (the `.jpg` literal is dropped).
    ("get", "/Videos/{itemId}/Trickplay/{width}/tiles.m3u8"),
    ("get", "/Videos/{itemId}/Trickplay/{width}/{index}"),
    // Lyrics (get/upload/delete + remote search/download/fetch).
    ("get", "/Audio/{itemId}/Lyrics"),
    ("post", "/Audio/{itemId}/Lyrics"),
    ("delete", "/Audio/{itemId}/Lyrics"),
    ("get", "/Audio/{itemId}/RemoteSearch/Lyrics"),
    ("post", "/Audio/{itemId}/RemoteSearch/Lyrics/{lyricId}"),
    ("get", "/Providers/Lyrics/{lyricId}"),
    // Subtitles (delete stored / upload / remote search+download / fetch). The
    // two `/Items/{itemId}/RemoteSearch/Subtitles/{X}` routes normalize to the
    // same `{language}` path but differ by method (GET search vs POST download).
    ("post", "/Videos/{itemId}/Subtitles"),
    ("delete", "/Videos/{itemId}/Subtitles/{index}"),
    ("get", "/Items/{itemId}/RemoteSearch/Subtitles/{language}"),
    ("post", "/Items/{itemId}/RemoteSearch/Subtitles/{language}"),
    ("get", "/Providers/Subtitles/Subtitles/{subtitleId}"),
    ("post", "/Jellyfin.Plugin.OpenSubtitles/ValidateLoginInfo"),
    // On-the-fly subtitle conversion (SubtitleEncoder seam) + FallbackFont
    // (encoding-options config seam + FileSystem). The `Stream.{format}` routes
    // normalize to a `{routeFormat}` capture; the ticks route adds a second one.
    (
        "get",
        "/Videos/{itemId}/{container}/Subtitles/{index}/subtitles.m3u8",
    ),
    (
        "get",
        "/Videos/{itemId}/{container}/Subtitles/{index}/{routeFormat}",
    ),
    (
        "get",
        "/Videos/{itemId}/{container}/Subtitles/{index}/{routeFormat}/{routeFormat}",
    ),
    ("get", "/FallbackFont/Fonts"),
    ("get", "/FallbackFont/Fonts/{name}"),
    // Deferred (stay on the 501 stub): `/MediaSegmentsApi/*` (plugin host).
    // Batch 12 — Devices + ApiKeys + ClientLog.
    ("get", "/Devices"),
    ("delete", "/Devices"),
    ("get", "/Devices/Info"),
    ("get", "/Devices/Options"),
    ("post", "/Devices/Options"),
    ("get", "/Auth/Keys"),
    ("post", "/Auth/Keys"),
    ("delete", "/Auth/Keys/{key}"),
    ("post", "/ClientLog/Document"),
    // Batch 13 — System admin + Configuration + Branding + Localization +
    // DisplayPreferences + ActivityLog + Dashboard + Environment + TimeSync.
    // System info/lifecycle/logs/endpoint (Info + Info/Public are First-Light).
    ("get", "/System/Info/Storage"),
    ("get", "/System/Ping"),
    ("post", "/System/Ping"),
    ("post", "/System/Restart"),
    ("post", "/System/Shutdown"),
    ("get", "/System/Logs"),
    ("get", "/System/Logs/Log"),
    ("get", "/System/Endpoint"),
    // Configuration (read/write + named + metadata defaults + branding write).
    ("get", "/System/Configuration"),
    ("post", "/System/Configuration"),
    ("get", "/System/Configuration/MetadataOptions/Default"),
    ("post", "/System/Configuration/Branding"),
    ("get", "/System/Configuration/{key}"),
    ("post", "/System/Configuration/{key}"),
    // Activity log.
    ("get", "/System/ActivityLog/Entries"),
    // Branding reads (Splashscreen GET/POST/DELETE are real in Batch 16).
    ("get", "/Branding/Configuration"),
    ("get", "/Branding/Css"),
    ("get", "/Branding/Css.css"),
    // Localization.
    ("get", "/Localization/Cultures"),
    ("get", "/Localization/Countries"),
    ("get", "/Localization/ParentalRatings"),
    ("get", "/Localization/Options"),
    // Display preferences.
    ("get", "/DisplayPreferences/{displayPreferencesId}"),
    ("post", "/DisplayPreferences/{displayPreferencesId}"),
    // Dashboard configuration pages.
    ("get", "/web/ConfigurationPages"),
    ("get", "/web/ConfigurationPage"),
    // Environment filesystem browse.
    ("get", "/Environment/DirectoryContents"),
    ("post", "/Environment/ValidatePath"),
    ("get", "/Environment/Drives"),
    ("get", "/Environment/NetworkShares"),
    ("get", "/Environment/ParentPath"),
    ("get", "/Environment/DefaultDirectoryBrowser"),
    // Time sync.
    ("get", "/GetUtcTime"),
    // Batch 14 — Library reads/serve/scan + item external-id descriptors.
    // Theme media (songs/videos/combined), the original-file serve, and the
    // library-scan trigger. See `handlers::library` / `handlers::item_lookup`.
    ("get", "/Items/{itemId}/ThemeSongs"),
    ("get", "/Items/{itemId}/ThemeVideos"),
    ("get", "/Items/{itemId}/ThemeMedia"),
    ("get", "/Items/{itemId}/File"),
    ("post", "/Library/Refresh"),
    ("get", "/Items/{itemId}/ExternalIdInfos"),
    // Batch 4 — remote metadata search + apply (`ItemLookupController`). Each
    // typed search route collapses its `RemoteSearchQuery<XInfo>` into the
    // object-safe `ProviderManager::remote_search` seam; the remote fetchers
    // (TMDb/TVDb/MusicBrainz) are deferred, so with none registered the search
    // faithfully returns `[]`. Apply resolves the item then drives the real
    // `refresh_full_item` seam (the refresh pipeline is the deferred piece).
    ("post", "/Items/RemoteSearch/Movie"),
    ("post", "/Items/RemoteSearch/Trailer"),
    ("post", "/Items/RemoteSearch/MusicVideo"),
    ("post", "/Items/RemoteSearch/Series"),
    ("post", "/Items/RemoteSearch/BoxSet"),
    ("post", "/Items/RemoteSearch/MusicArtist"),
    ("post", "/Items/RemoteSearch/MusicAlbum"),
    ("post", "/Items/RemoteSearch/Person"),
    ("post", "/Items/RemoteSearch/Book"),
    ("post", "/Items/RemoteSearch/Apply/{itemId}"),
    // Batch 2 (this unit) — filesystem-monitor change-report webhooks
    // (`LibraryController.PostUpdated{Series,Movies,Media}`). Each selects the
    // affected items (Series by TVDB id; Movies by IMDb/TMDb id; Media by the
    // supplied update paths) and reports every path to the `LibraryMonitor` seam
    // on `AppState`. See `handlers::library`.
    ("post", "/Library/Series/Added"),
    ("post", "/Library/Series/Updated"),
    ("post", "/Library/Movies/Added"),
    ("post", "/Library/Movies/Updated"),
    ("post", "/Library/Media/Updated"),
    // Batch 15 — ScheduledTasks read/run.
    // List, fetch-by-id, and manual run-now. The scheduler-cron machinery stays
    // on the 501 stub: `DELETE /ScheduledTasks/Running/{taskId}` (cancel — no
    // background run to cancel) and `POST /ScheduledTasks/{taskId}/Triggers`
    // (trigger-config persistence). Channels (`ChannelsController`) stays fully
    // on the 501 stub as a deferred Live-TV/channels subsystem.
    ("get", "/ScheduledTasks"),
    ("get", "/ScheduledTasks/{taskId}"),
    ("post", "/ScheduledTasks/Running/{taskId}"),
    // Batch 16 — the last portable stubs.
    // Similar-items aliases (`LibraryController.GetSimilarItems`); `Shows/…/Similar`
    // is already real in Batch 8.
    ("get", "/Albums/{itemId}/Similar"),
    ("get", "/Artists/{itemId}/Similar"),
    ("get", "/Items/{itemId}/Similar"),
    ("get", "/Movies/{itemId}/Similar"),
    ("get", "/Trailers/{itemId}/Similar"),
    // Image write side (`ImageController` upload/delete). The indexed HEAD-Genres
    // and GET-MusicGenres by-name *read* variants are already listed above in the
    // by-name image block; do not re-add them here or REAL_ROUTES gains duplicate
    // rows (they are harmless to the router, which mounts by handler membership,
    // but they inflate the route count and mislead the contract accounting).
    ("post", "/Items/{itemId}/Images/{imageType}"),
    ("delete", "/Items/{itemId}/Images/{imageType}"),
    ("post", "/Items/{itemId}/Images/{imageType}/{imageIndex}"),
    ("delete", "/Items/{itemId}/Images/{imageType}/{imageIndex}"),
    (
        "post",
        "/Items/{itemId}/Images/{imageType}/{imageIndex}/Index",
    ),
    ("post", "/UserImage"),
    // Scheduler cancel + trigger-config (`ScheduledTasksController`).
    ("delete", "/ScheduledTasks/Running/{taskId}"),
    ("post", "/ScheduledTasks/{taskId}/Triggers"),
    // Metadata-editor descriptor + user-view grouping options.
    ("get", "/Items/{itemId}/MetadataEditor"),
    ("get", "/UserViews/GroupingOptions"),
    // Batch 6 — portable extras: TMDb client image configuration
    // (`TmdbController`); served static while the live TMDb provider is deferred.
    ("get", "/Tmdb/ClientConfiguration"),
    // MergeVersions plugin — bulk merge/split of duplicate versions across the
    // whole library, ported onto the core `PrimaryVersionId` version-group seam
    // (the same `LibraryManager` merge/split logic that backs the in-tree
    // `POST /Videos/MergeVersions`); no dynamic plugin host required. The
    // parameterless routes each scan the library. See `handlers::merge_versions`.
    ("post", "/MergeVersions/MergeMovies"),
    ("post", "/MergeVersions/SplitMovies"),
    ("post", "/MergeVersions/MergeEpisodes"),
    ("post", "/MergeVersions/SplitEpisodes"),
    // Deferred — third-party PLUGIN routes with no core-Jellyfin controller to
    // port from (they need the un-ported dynamic plugin host); they stay on the
    // `501` stub: `GET|POST /Episode/{Id}/Timestamps` (the
    // `IntroSkipper`/`SkipIntro` plugin) and `/MediaSegmentsApi/*` (the
    // `SegmentEditor` plugin).
    // Library structure — the media-folder listing (`LibraryController`).
    ("get", "/Library/MediaFolders"),
    // Batch 1 (this unit) — Library admin / virtual folders. The
    // filesystem-backed `LibraryStructureController` CRUD + the two
    // `LibraryController` structure reads, over the `VirtualFolderManager` seam
    // (see `handlers::library_structure` / `handlers::library`). No route here
    // stays on the 501 stub; `AvailableOptions` returns empty provider lists
    // faithfully (no metadata plugins registered at this seam).
    ("get", "/Library/VirtualFolders"),
    ("post", "/Library/VirtualFolders"),
    ("delete", "/Library/VirtualFolders"),
    ("post", "/Library/VirtualFolders/Name"),
    ("post", "/Library/VirtualFolders/LibraryOptions"),
    ("post", "/Library/VirtualFolders/Paths"),
    ("post", "/Library/VirtualFolders/Paths/Update"),
    ("delete", "/Library/VirtualFolders/Paths"),
    ("get", "/Library/PhysicalPaths"),
    ("get", "/Libraries/AvailableOptions"),
    // Branding splashscreen (`ImageController`).
    ("get", "/Branding/Splashscreen"),
    ("post", "/Branding/Splashscreen"),
    ("delete", "/Branding/Splashscreen"),
    // Plugins Tier 1 — the plugin-manager surface (`PluginsController` +
    // `PackageController`) over the compile-time plugin registry. Reads,
    // enable/disable, config, and the repository list are real; install and
    // uninstall are honest rejections (they need the Tier-2 dynamic host, not a
    // faked success). See `handlers::plugins`.
    ("get", "/Plugins"),
    ("get", "/Plugins/{pluginId}/Configuration"),
    ("post", "/Plugins/{pluginId}/Configuration"),
    ("post", "/Plugins/{pluginId}/{version}/Enable"),
    ("post", "/Plugins/{pluginId}/{version}/Disable"),
    ("delete", "/Plugins/{pluginId}"),
    ("delete", "/Plugins/{pluginId}/{version}"),
    ("get", "/Plugins/{pluginId}/{version}/Image"),
    ("post", "/Plugins/{pluginId}/Manifest"),
    ("get", "/Repositories"),
    ("post", "/Repositories"),
    ("get", "/Packages"),
    ("get", "/Packages/{name}"),
    ("post", "/Packages/Installed/{name}"),
    ("delete", "/Packages/Installing/{packageId}"),
    // Live TV read surface (empty/disabled state; mutations stay on the 501 stub).
    ("get", "/LiveTv/Info"),
    ("get", "/LiveTv/GuideInfo"),
    ("get", "/LiveTv/Channels"),
    ("get", "/LiveTv/Channels/{channelId}"),
    ("get", "/LiveTv/Programs"),
    ("post", "/LiveTv/Programs"),
    ("get", "/LiveTv/Programs/{programId}"),
    ("get", "/LiveTv/Programs/Recommended"),
    ("get", "/LiveTv/Recordings"),
    ("get", "/LiveTv/Recordings/{recordingId}"),
    ("delete", "/LiveTv/Recordings/{recordingId}"),
    ("get", "/LiveTv/Recordings/Folders"),
    ("get", "/LiveTv/Recordings/Groups"),
    ("get", "/LiveTv/Recordings/Groups/{groupId}"),
    ("get", "/LiveTv/Recordings/Series"),
    ("get", "/LiveTv/LiveRecordings/{recordingId}/stream"),
    ("get", "/LiveTv/LiveStreamFiles/{streamId}/{container}"),
    ("get", "/LiveTv/Timers"),
    ("post", "/LiveTv/Timers"),
    ("get", "/LiveTv/Timers/{timerId}"),
    ("post", "/LiveTv/Timers/{timerId}"),
    ("delete", "/LiveTv/Timers/{timerId}"),
    ("get", "/LiveTv/Timers/Defaults"),
    ("get", "/LiveTv/SeriesTimers"),
    ("post", "/LiveTv/SeriesTimers"),
    ("get", "/LiveTv/SeriesTimers/{timerId}"),
    ("post", "/LiveTv/SeriesTimers/{timerId}"),
    ("delete", "/LiveTv/SeriesTimers/{timerId}"),
    ("get", "/LiveTv/ChannelMappingOptions"),
    ("post", "/LiveTv/ChannelMappings"),
    ("post", "/LiveTv/ListingProviders"),
    ("delete", "/LiveTv/ListingProviders"),
    ("get", "/LiveTv/ListingProviders/Default"),
    ("get", "/LiveTv/ListingProviders/SchedulesDirect/Countries"),
    ("get", "/LiveTv/ListingProviders/Lineups"),
    ("post", "/LiveTv/TunerHosts"),
    ("delete", "/LiveTv/TunerHosts"),
    ("get", "/LiveTv/TunerHosts/Types"),
    ("get", "/LiveTv/Tuners/Discover"),
    ("get", "/LiveTv/Tuners/Discvover"),
    ("post", "/LiveTv/Tuners/{tunerId}/Reset"),
    // SyncPlay — synchronized group playback.
    ("post", "/SyncPlay/New"),
    ("post", "/SyncPlay/Join"),
    ("post", "/SyncPlay/Leave"),
    ("get", "/SyncPlay/List"),
    ("get", "/SyncPlay/{id}"),
    ("post", "/SyncPlay/SetNewQueue"),
    ("post", "/SyncPlay/SetPlaylistItem"),
    ("post", "/SyncPlay/RemoveFromPlaylist"),
    ("post", "/SyncPlay/MovePlaylistItem"),
    ("post", "/SyncPlay/Queue"),
    ("post", "/SyncPlay/Unpause"),
    ("post", "/SyncPlay/Pause"),
    ("post", "/SyncPlay/Stop"),
    ("post", "/SyncPlay/Seek"),
    ("post", "/SyncPlay/Buffering"),
    ("post", "/SyncPlay/Ready"),
    ("post", "/SyncPlay/SetIgnoreWait"),
    ("post", "/SyncPlay/NextItem"),
    ("post", "/SyncPlay/PreviousItem"),
    ("post", "/SyncPlay/SetRepeatMode"),
    ("post", "/SyncPlay/SetShuffleMode"),
    ("post", "/SyncPlay/Ping"),
    // Intro Skipper extension — the plugin's five controllers plus the
    // FileTransformation registration hook it depends on. See
    // `handlers::intro_skipper`. Paths are the contract-canonical forms
    // (`/MediaSegmentsApi/{segmentId}` canonicalizes to `{itemId}`).
    ("get", "/Episode/{Id}/Timestamps"),
    ("post", "/Episode/{Id}/Timestamps"),
    ("get", "/Episode/{Id}/IntroSkipperSegments"),
    ("post", "/Intros/EraseTimestamps"),
    ("post", "/Intros/RebuildDatabase"),
    ("get", "/MediaSegmentsApi"),
    ("post", "/MediaSegmentsApi/{itemId}"),
    ("delete", "/MediaSegmentsApi/{itemId}"),
    ("post", "/SkipButtonCss/InjectCss"),
    ("post", "/SkipButtonCss/UpdateSkipDuration"),
    ("get", "/IntroSkipper"),
    ("get", "/IntroSkipper/SupportBundle"),
    ("get", "/Intros/AnalyzerActions/{SeasonId}"),
    ("post", "/Intros/AnalyzerActions/UpdateSeason"),
    ("get", "/Intros/Show/{SeriesId}/{SeasonId}"),
    ("delete", "/Intros/Show/{SeriesId}/{SeasonId}"),
    ("post", "/Intros/ScanSeason/{SeriesId}/{SeasonId}"),
    ("get", "/Intros/ScanStatus"),
    ("post", "/FileTransformation/RegisterTransformation"),
];

/// Mounts every real First-Light handler onto `router`, overriding the matching
/// `501` stub entries registered from the vendored contract table.
///
/// Called by [`create_router`](crate::router::create_router) after the stub loop
/// so a `(method, path)` with a real handler wins over its stub.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    let router = users::register(router);
    let router = user_views::register(router);
    let router = items::register(router);
    let router = item_update::register(router);
    let router = media_info::register(router);
    let router = videos::register(router);
    let router = audio::register(router);
    let router = hls::register(router);
    let router = images::register(router);
    let router = genres::register(router);
    let router = music_genres::register(router);
    let router = studios::register(router);
    let router = persons::register(router);
    let router = artists::register(router);
    let router = years::register(router);
    let router = filter::register(router);
    let router = suggestions::register(router);
    let router = instant_mix::register(router);
    let router = intro_skipper::register(router);
    let router = movies::register(router);
    let router = trailers::register(router);
    let router = user_library::register(router);
    let router = playstate::register(router);
    let router = session::register(router);
    let router = startup::register(router);
    let router = quick_connect::register(router);
    let router = playlists::register(router);
    let router = collection::register(router);
    let router = tv_shows::register(router);
    let router = remote_images::register(router);
    let router = media_segments::register(router);
    let router = trickplay::register(router);
    let router = lyrics::register(router);
    let router = subtitles::register(router);
    let router = devices::register(router);
    let router = api_key::register(router);
    let router = client_log::register(router);
    let router = search::register(router);
    let router = channels::register(router);
    let router = backup::register(router);
    // Batch 13 — system admin / configuration / branding / localization /
    // display preferences / activity log / dashboard / environment / time sync.
    let router = config::register(router);
    let router = branding::register(router);
    let router = localization::register(router);
    let router = display_preferences::register(router);
    let router = activity_log::register(router);
    let router = dashboard::register(router);
    let router = environment::register(router);
    let router = time_sync::register(router);
    // Batch 14 — library reads/serve/scan + item external-id descriptors.
    let router = library::register(router);
    // Batch 1 — library admin / virtual folders (LibraryStructureController).
    let router = library_structure::register(router);
    let router = item_lookup::register(router);
    let router = system::register(router);
    // Batch 15 — ScheduledTasks read/run.
    let router = scheduled_tasks::register(router);
    // Batch 6 — portable extras: TMDb client config (TmdbController).
    let router = tmdb::register(router);
    // Batch 16 — the last portable stubs.
    let router = similar::register(router);
    // MergeVersions plugin — bulk merge/split of duplicate versions.
    let router = merge_versions::register(router);
    // Plugins Tier 1 — plugin-manager surface over the compile-time registry.
    let router = plugins::register(router);
    // Live TV read surface — empty/disabled state so the web UI treats Live TV
    // as "not configured" instead of erroring on 501 (mutations stay stubbed).
    let router = live_tv::register(router);
    // SyncPlay — synchronized group playback over the session WebSocket.
    let router = sync_play::register(router);
    // The session WebSocket (`/socket`) — not in the OpenAPI contract; jellyfin-web
    // needs it to establish a connection or it reports "Connection Failure".
    websocket::register(router)
}
