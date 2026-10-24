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
/// directory service: `Default` modes, nothing replaced). A file item refreshes
/// through the library scan of its path, whose probe re-runs because its
/// sidecar set changed (`ProbeProvider.HasChanged`); an item with no file of
/// its own through the provider manager.
pub(crate) async fn queue_high_priority_refresh(
    state: &AppState,
    item_id: Uuid,
) -> Result<(), ApiError> {
    use ferrofin_traits::library::ScanTarget;
    use ferrofin_traits::providers::{MetadataRefreshOptions, RefreshPriority};
    let options = MetadataRefreshOptions::default();
    let item = state.library.get_item_by_id(item_id).await?;
    if let Some(path) = item.as_ref().and_then(refresh_scan_path) {
        state
            .library
            .queue_refresh_scan(ScanTarget::Items(vec![path]), &options)
            .await?;
        return Ok(());
    }
    state
        .providers
        .queue_refresh(item_id, &options, RefreshPriority::High)
        .await?;
    Ok(())
}

/// Runs an item's default metadata refresh and returns once it has run —
/// `item.RefreshMetadata(CancellationToken.None)`, which a handler awaits
/// rather than queues. A file item refreshes through the scan queue's
/// priority lane ([`LibraryManager::run_refresh_scan`]), on a task of its own
/// so a client that hangs up does not withdraw it; an item with no file of
/// its own is queued as [`queue_high_priority_refresh`] does.
///
/// [`LibraryManager::run_refresh_scan`]: ferrofin_traits::library::LibraryManager::run_refresh_scan
pub(crate) async fn run_item_refresh(state: &AppState, item_id: Uuid) -> Result<(), ApiError> {
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::library::ScanTarget;
    use ferrofin_traits::providers::MetadataRefreshOptions;
    let item = state.library.get_item_by_id(item_id).await?;
    let Some(path) = item.as_ref().and_then(refresh_scan_path) else {
        return queue_high_priority_refresh(state, item_id).await;
    };
    let library = std::sync::Arc::clone(&state.library);
    // In the request's span: the request awaits it (LOGGING.md §4).
    let ran = tokio::spawn(tracing::Instrument::in_current_span(async move {
        library
            .run_refresh_scan(
                ScanTarget::Items(vec![path]),
                &MetadataRefreshOptions::default(),
            )
            .await
    }))
    .await
    .map_err(|err| {
        ApiError::Service(ServiceError::backend(format!(
            "item refresh task failed: {err}"
        )))
    })??;
    if !ran {
        return Err(ApiError::ServiceUnavailable(format!(
            "the refresh of item {item_id} was stopped before it finished"
        )));
    }
    Ok(())
}

/// Whether the library scan can reach `target`: every path of it lies under
/// a location of a configured library (the scan plans nothing anywhere
/// else). A library or the root always can.
pub(crate) async fn scan_reaches(
    state: &AppState,
    target: &ferrofin_traits::library::ScanTarget,
) -> Result<bool, ApiError> {
    use ferrofin_traits::library::ScanTarget;
    let (ScanTarget::Paths(paths) | ScanTarget::Items(paths)) = target else {
        return Ok(true);
    };
    let folders = state.virtual_folders.get_virtual_folders().await?;
    Ok(paths.iter().all(|path| {
        folders.iter().any(|folder| {
            folder
                .locations
                .iter()
                .any(|location| path_is_under(path, location))
        })
    }))
}

/// Whether `path` is `root` or lies under it, component-wise.
fn path_is_under(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The path an item refreshes through the library scan by: its own
/// filesystem path, when it has one inside a library. `None` for an item
/// with no file of its own — a person, a genre or studio, a channel or
/// live-TV item streamed from a URL, a row outside every library — which the
/// provider manager refreshes.
pub(crate) fn refresh_scan_path(
    item: &ferrofin_db::entities::base_items::BaseItemEntity,
) -> Option<String> {
    let in_library = item
        .top_parent_id
        .as_deref()
        .is_some_and(|top| !top.is_empty());
    item.path
        .clone()
        .filter(|path| in_library && !path.is_empty() && !path.contains("://"))
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
mod item_dates;
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
    // Media segments (read). The `SegmentEditor` plugin's `/MediaSegmentsApi/*`
    // write routes are served by the intro-skipper extension (listed below).
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
    // object-safe `ProviderManager::remote_search` seam, which fans out over
    // the registered fetchers (TMDB/TVDB/OMDb/MusicBrainz/TheAudioDb). Apply
    // replaces the item's ids with the chosen result's and refreshes it: a
    // file item or folder through the library scan of its path, anything
    // else through `refresh_full_item`.
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
    // Similar-items aliases (`LibraryController.GetSimilarItems`); the
    // `Shows/…/Similar` row sits in Batch 8, though `handlers::similar` serves it.
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
    // The third-party PLUGIN routes with no core-Jellyfin controller
    // (`/Episode/{Id}/Timestamps`, `/MediaSegmentsApi/*`) are served by the
    // compiled-in intro-skipper extension — see `EXTENSION_ROUTES` below.
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
    // NOTE: the runtime plugins' URL space `/Plugins/{pluginId}/web/{*path}`
    // is registered by handlers::plugins but deliberately NOT listed here —
    // REAL_ROUTES rows must be vendored-contract operations, and that route
    // is a Ferrofin extension (like /health), outside the Jellyfin spec.
    ("get", "/Intros/AnalyzerActions/{SeasonId}"),
    ("post", "/Intros/AnalyzerActions/UpdateSeason"),
    ("get", "/Intros/Show/{SeriesId}/{SeasonId}"),
    ("delete", "/Intros/Show/{SeriesId}/{SeasonId}"),
    ("post", "/Intros/ScanSeason/{SeriesId}/{SeasonId}"),
    ("get", "/Intros/ScanStatus"),
    ("post", "/FileTransformation/RegisterTransformation"),
];

/// `(method, path, extension-id)` for every route owned by a **compiled-in
/// extension** (`ferrofin-extensions`); core routes are everything in
/// [`REAL_ROUTES`] not listed here.
///
/// This is the machine-readable ownership manifest, so "how complete is core
/// Ferrofin" is separable from "how complete is each extension" (the parity
/// printout in `tests/contract_superset.rs` reads it). Extension authors
/// update ONE place: add the route here and to `REAL_ROUTES`; the compile-time
/// assertion below catches an entry that drifts from (or never lands in)
/// `REAL_ROUTES`.
///
/// Paths are the contract-canonical forms, verbatim from `REAL_ROUTES`
/// (e.g. `/MediaSegmentsApi/{segmentId}` canonicalizes to `{itemId}`).
pub const EXTENSION_ROUTES: &[(&str, &str, &str)] = &[
    // MergeVersions plugin (see handlers::merge_versions).
    ("post", "/MergeVersions/MergeMovies", "merge-versions"),
    ("post", "/MergeVersions/SplitMovies", "merge-versions"),
    ("post", "/MergeVersions/MergeEpisodes", "merge-versions"),
    ("post", "/MergeVersions/SplitEpisodes", "merge-versions"),
    // Intro Skipper extension (see handlers::intro_skipper).
    ("get", "/Episode/{Id}/Timestamps", "intro-skipper"),
    ("post", "/Episode/{Id}/Timestamps", "intro-skipper"),
    ("get", "/Episode/{Id}/IntroSkipperSegments", "intro-skipper"),
    ("post", "/Intros/EraseTimestamps", "intro-skipper"),
    ("post", "/Intros/RebuildDatabase", "intro-skipper"),
    ("get", "/MediaSegmentsApi", "intro-skipper"),
    ("post", "/MediaSegmentsApi/{itemId}", "intro-skipper"),
    ("delete", "/MediaSegmentsApi/{itemId}", "intro-skipper"),
    ("post", "/SkipButtonCss/InjectCss", "intro-skipper"),
    ("post", "/SkipButtonCss/UpdateSkipDuration", "intro-skipper"),
    ("get", "/IntroSkipper", "intro-skipper"),
    ("get", "/IntroSkipper/SupportBundle", "intro-skipper"),
    ("get", "/Intros/AnalyzerActions/{SeasonId}", "intro-skipper"),
    (
        "post",
        "/Intros/AnalyzerActions/UpdateSeason",
        "intro-skipper",
    ),
    ("get", "/Intros/Show/{SeriesId}/{SeasonId}", "intro-skipper"),
    (
        "delete",
        "/Intros/Show/{SeriesId}/{SeasonId}",
        "intro-skipper",
    ),
    (
        "post",
        "/Intros/ScanSeason/{SeriesId}/{SeasonId}",
        "intro-skipper",
    ),
    ("get", "/Intros/ScanStatus", "intro-skipper"),
    // FileTransformation registration hook (lives in handlers::intro_skipper).
    (
        "post",
        "/FileTransformation/RegisterTransformation",
        "file-transformation",
    ),
];

/// The upstream Jellyfin release behaviour is ported from and every [`VERIFIED`]
/// row is compared against, except the rows in [`VERIFIED_AGAINST_EARLIER_PIN`].
/// It is one fact for the whole record, so no row can carry a wrong tag or commit.
/// Owner decisions: Jellyfin 12 (10.12 in all but name) is the source of truth,
/// and where 10.11.8 differs from it, 10.11.8 is the one that is wrong
/// (2026-09-02). The pin is always the **latest Jellyfin release**, never a
/// release candidate: when a newer one ships, move this tag and the commit to
/// it, set [`UPSTREAM_PIN_DATE`] to that day, and list every row not already
/// listed in [`VERIFIED_AGAINST_EARLIER_PIN`] (or, for a subsystem row,
/// [`VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN`]) under the old tag (2026-10-16).
/// The vendored OpenAPI contract stays 10.11.8: that is the API clients speak,
/// even though Jellyfin 12 dropped operations from it.
pub const UPSTREAM_TAG: &str = "v12.2";
/// The commit `UPSTREAM_TAG` resolves to in `github.com/jellyfin/jellyfin`.
pub const UPSTREAM_COMMIT: &str = "ca0f16eb7c";
/// The day [`UPSTREAM_TAG`] became the pin, `YYYY-MM-DD`; move it with the tag.
/// A row dated on or after it was compared against [`UPSTREAM_TAG`], so it can
/// never be listed in [`VERIFIED_AGAINST_EARLIER_PIN`] or
/// [`VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN`].
pub const UPSTREAM_PIN_DATE: &str = "2026-10-16";

/// [`VERIFIED`] rows last compared against an earlier pin, as
/// `(method, path, tag compared against)`. They are completed verification work,
/// but not against [`UPSTREAM_TAG`], and the parity line counts them apart.
/// Re-compare the row against [`UPSTREAM_TAG`], update its `date`, and remove it
/// from this list. `v12.0-rc7` is commit `4910aafa1a`.
pub const VERIFIED_AGAINST_EARLIER_PIN: &[(&str, &str, &str)] = &[];

/// [`VERIFIED_SUBSYSTEMS`] rows last compared against an earlier pin, as
/// `(key, tag compared against)` — the same rules as
/// [`VERIFIED_AGAINST_EARLIER_PIN`].
pub const VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN: &[(&str, &str)] = &[];

/// The subsystems a [`VERIFIED_SUBSYSTEMS`] row may cover, by key. Every
/// operation crosses several of them, so one row states their divergences once
/// and operation rows name it in [`Verified::refs`].
pub const SUBSYSTEM_KEYS: &[&str] = &[
    "query-translation",
    "dto-fields",
    "user-data",
    "access-filtering",
    "images",
    "paging-sort",
];

/// One shared subsystem whose Ferrofin implementation was compared against the
/// upstream Jellyfin C# — see [`VERIFIED_SUBSYSTEMS`].
#[derive(Debug)]
pub struct VerifiedSubsystem {
    /// One of [`SUBSYSTEM_KEYS`], unique in [`VERIFIED_SUBSYSTEMS`].
    pub key: &'static str,
    /// The C# file compared, relative to the upstream repository root —
    /// `"Jellyfin.Server.Implementations/Item/BaseItemRepository.cs"`.
    pub upstream_file: &'static str,
    /// The method in that file the comparison centres on — `"TranslateQuery"`.
    pub upstream_method: &'static str,
    /// The Ferrofin code path compared, for the next reader.
    pub ferrofin: &'static str,
    /// Date of the comparison, `YYYY-MM-DD`.
    pub date: &'static str,
    /// Accepted divergences, one sentence each, under the same rules as
    /// [`Verified::divergences`].
    pub divergences: &'static [&'static str],
}

/// **Per-subsystem parity rows.** A row exists only once its subsystem was
/// compared against the upstream C# at [`UPSTREAM_TAG`] (or the earlier tag
/// [`VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN`] names), under the rules of
/// [`VERIFIED`]. Subsystem rows never count towards the operation parity line.
pub const VERIFIED_SUBSYSTEMS: &[VerifiedSubsystem] = &[];

/// One contract operation whose Ferrofin implementation was compared against
/// the upstream Jellyfin C# for behavioral equivalence — see [`VERIFIED`].
#[derive(Debug)]
pub struct Verified {
    /// Lower-case HTTP method, exactly as in [`REAL_ROUTES`].
    pub method: &'static str,
    /// Contract path, exactly as in [`REAL_ROUTES`].
    pub path: &'static str,
    /// The C# file compared, relative to the upstream repository root —
    /// `"Jellyfin.Api/Controllers/UserLibraryController.cs"`.
    pub upstream_file: &'static str,
    /// The controller method in that file — `"GetItem"`.
    pub upstream_method: &'static str,
    /// The Ferrofin code path compared (handler → core), for the next reader —
    /// `"handlers/user_library.rs get_item → ferrofin-core dto_service.rs"`.
    pub ferrofin: &'static str,
    /// Date of the comparison, `YYYY-MM-DD`.
    pub date: &'static str,
    /// Accepted divergences, one sentence each. Owner-scope items (native .NET
    /// plugin loading, SSDP) and Jellyfin bugs deliberately
    /// not ported belong here; anything else is a gap to fix before the row is
    /// written. An entry with divergences still counts as verified — the text
    /// is the published statement of what is *not* done.
    pub divergences: &'static [&'static str],
    /// Keys of the [`VERIFIED_SUBSYSTEMS`] rows this operation relies on, whose
    /// divergences apply here too and are not repeated in `divergences`.
    pub refs: &'static [&'static str],
}

/// **The API-parity record.** One row per contract operation whose Ferrofin
/// implementation has been compared against the upstream Jellyfin C#
/// ([`UPSTREAM_TAG`], or the earlier tag [`VERIFIED_AGAINST_EARLIER_PIN`]
/// names for that row) for behavioral equivalence: parameters honored and their
/// defaults, filters, sort, paging, DTO field set and nullability, status
/// codes, side effects, auth rules.
///
/// Rules, in full:
/// - a row exists ⇔ the operation is deep-verified; the only other state is
///   being listed in [`VERIFIED_AGAINST_EARLIER_PIN`] (verified against an
///   earlier pin, owed a re-check), and there is no other published count
///   (runtime probes are supporting evidence only);
/// - rows are only ever added or amended — removing one is a visible deletion
///   in review, and a benchmark-suite rewrite cannot touch this file;
/// - `tests/contract_superset.rs` checks every row is a [`REAL_ROUTES`]
///   operation with a well-formed `upstream_file`/`upstream_method`/`date`,
///   checks every [`VERIFIED_AGAINST_EARLIER_PIN`] entry is a distinct row with
///   a tag other than [`UPSTREAM_TAG`] dated before [`UPSTREAM_PIN_DATE`],
///   checks every `refs` key names a [`VERIFIED_SUBSYSTEMS`] row (which obey
///   the same rules), and — with `FERROFIN_UPSTREAM_DIR` naming a Jellyfin
///   checkout at [`UPSTREAM_COMMIT`] — checks each row's `upstream_file`
///   exists there and contains `upstream_method`, and prints `N / 412 operations
///   deep-verified against <UPSTREAM_TAG>, plus M compared against an earlier
///   release` plus the per-controller table — that printout is the README
///   parity section. N moves when a row is written or re-checked, and when the
///   pin moves (its rows then become M); N reaches 100 % when the work is done.
pub const VERIFIED: &[Verified] = &[
    Verified {
        method: "get",
        path: "/Shows/NextUp",
        upstream_file: "Jellyfin.Api/Controllers/TvShowsController.cs",
        upstream_method: "GetNextUp",
        ferrofin: "handlers/tv_shows.rs get_next_up → ferrofin-core tv_series_manager.rs \
               (TVSeriesManager) → next_up_service.rs (NextUpService)",
        date: "2026-10-19",
        divergences: &[
            "TVSeriesManager.GetPreferredVersion (continue in the alternate version the user \
         has been watching, matched by media-source name) is not ported: the pick is always \
         the primary episode row. Un-defer path: v12's Video.GetMediaSourceName \
         common-prefix naming is ported (video_versions::media_source_name); swap by that \
         name in tv_series_manager (see the TODO there).",
            "parentId naming a non-view item: v12 routes it to AncestorIds, which the keys \
         statement ignores, so the answer is always empty; Ferrofin scopes by TopParentId = \
         that item (a physical library folder answers, anything else is empty as upstream). \
         Jellyfin bug not ported.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Artists",
        upstream_file: "Jellyfin.Api/Controllers/ArtistsController.cs",
        upstream_method: "GetArtists",
        ferrofin: "handlers/artists.rs get_artists → handlers/by_name.rs (ByNameListQuery \
               artists_query, resolve_by_name_ids, project_query_result) → ferrofin-core \
               item_repository.rs get_artists (item_values_with_counts, \
               item_counts_by_clean_name)",
        date: "2026-10-23",
        divergences: BY_NAME_DIVERGENCES_ARTISTS,
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Artists/AlbumArtists",
        upstream_file: "Jellyfin.Api/Controllers/ArtistsController.cs",
        upstream_method: "GetAlbumArtists",
        ferrofin: "handlers/artists.rs get_album_artists → handlers/by_name.rs (artists_query, \
               resolve_by_name_ids) → ferrofin-core item_repository.rs get_album_artists \
               (item_values_with_counts)",
        date: "2026-10-23",
        divergences: BY_NAME_DIVERGENCES_ARTISTS,
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Genres",
        upstream_file: "Jellyfin.Api/Controllers/GenresController.cs",
        upstream_method: "GetGenres",
        ferrofin: "handlers/genres.rs get_genres (music/musicvideos library parent → \
               get_music_genres) → handlers/by_name.rs → ferrofin-core \
               item_repository.rs get_genres / get_music_genres (item_values_with_counts)",
        date: "2026-10-23",
        divergences: BY_NAME_DIVERGENCES_GENRES_STUDIOS,
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/MusicGenres",
        upstream_file: "Jellyfin.Api/Controllers/MusicGenresController.cs",
        upstream_method: "GetMusicGenres",
        ferrofin: "handlers/music_genres.rs get_music_genres → handlers/by_name.rs → \
               ferrofin-core item_repository.rs get_music_genres (item_values_with_counts)",
        date: "2026-10-23",
        divergences: BY_NAME_DIVERGENCES_GENRES_STUDIOS,
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Studios",
        upstream_file: "Jellyfin.Api/Controllers/StudiosController.cs",
        upstream_method: "GetStudios",
        ferrofin: "handlers/studios.rs get_studios → handlers/by_name.rs → ferrofin-core \
               item_repository.rs get_studios (item_values_with_counts)",
        date: "2026-10-23",
        divergences: BY_NAME_DIVERGENCES_GENRES_STUDIOS,
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/UserItems/Resume",
        upstream_file: "Jellyfin.Api/Controllers/ItemsController.cs",
        upstream_method: "GetResumeItems",
        ferrofin: "handlers/items.rs get_resume_items → ferrofin-core item_repository.rs \
               get_items → translate_query.rs (push_resumable_predicate, \
               push_order_expression DatePlayed)",
        date: "2026-10-19",
        divergences: RESUME_DIVERGENCES,
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/UserImage",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "PostUserImage",
        ferrofin: "handlers/images.rs post_user_image → handlers/items.rs resolve_user, \
               handlers/users.rs assert_can_update_user, handlers/image_upload.rs \
               (try_get_image_extension_from_content_type → ferrofin-model mime_types.rs \
               to_extension, decode_base64) → ferrofin-core user_manager.rs \
               save_profile_image",
        date: "2026-10-20",
        divergences: &[
            "The body is decoded in full before anything is written, so a body the decoder \
         rejects is a 400 that changes nothing. Upstream clears the old profile image row \
         first and decodes while copying, so invalid base64 fails mid-copy with a 500 \
         (FormatException) and leaves the user with no profile image. FromBase64Transform \
         also silently drops a trailing partial group (fewer than four characters): an \
         unpadded or short padded final group (`aGk`, `aG=`) loses its last bytes, and a \
         third `=` or a partial group after the padding is ignored. Ferrofin decodes the \
         final group in full and refuses the other two with a 400. Jellyfin bugs not \
         ported.",
            "The body is read as UTF-8 text before the user and the Content-Type are \
         checked, so a body that is not UTF-8 is the extractor's 400 whatever the user or \
         type; upstream resolves the user and checks the type first, and fails such a body \
         with a 500 while decoding. Jellyfin bug not ported.",
            "The body is capped by axum's implicit 2 MiB limit, where Kestrel accepts 30 MB. \
         Fix path: gl-24.",
        ],
        refs: &[],
    },
    Verified {
        method: "delete",
        path: "/UserImage",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "DeleteUserImage",
        ferrofin: "handlers/images.rs delete_user_image → handlers/items.rs resolve_user, \
               handlers/users.rs assert_can_update_user → ferrofin-core user_manager.rs \
               clear_profile_image",
        date: "2026-10-20",
        divergences: &[],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Audio/{itemId}/Lyrics",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "GetLyrics",
        ferrofin: "handlers/lyrics.rs get_lyrics → ferrofin-core lyric_manager.rs \
               get_lyrics (the item's Lyric streams, LrcLyricParser then TxtLyricParser)",
        date: "2026-10-19",
        divergences: &[
            "A Lyric stream whose file is gone (removed outside the server, before the next \
         refresh) is skipped and the next stream read; upstream's File.ReadAllTextAsync \
         throws and the request is a 500. Jellyfin bug not ported. The sidecar read is a \
         blocking std::fs read on the runtime (gl-144).",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Audio/{itemId}/Lyrics",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "UploadLyrics",
        ferrofin: "handlers/lyrics.rs upload_lyrics (upload_format, queue_high_priority_refresh) \
               → ferrofin-core lyric_manager.rs save_lyric (decode_stream_text, save_targets, \
               try_save_to_files)",
        date: "2026-10-19",
        divergences: &[
            "An upload with no Content-Length (a chunked body) is read and saved; upstream \
         tests Request.ContentLength and answers every chunked upload 400 \"No lyrics \
         uploaded\". Jellyfin bug not ported.",
            "The body is capped by axum's implicit 2 MiB limit, where Kestrel accepts 30 MB. \
         Fix path: gl-24.",
        ],
        refs: &[],
    },
    Verified {
        method: "delete",
        path: "/Audio/{itemId}/Lyrics",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "DeleteLyrics",
        ferrofin: "handlers/lyrics.rs delete_lyrics (run_item_refresh) → ferrofin-core \
               lyric_manager.rs delete_lyrics",
        date: "2026-10-19",
        divergences: &[],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Audio/{itemId}/RemoteSearch/Lyrics",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "SearchRemoteLyrics",
        ferrofin: "handlers/lyrics.rs search_remote_lyrics → ferrofin-core lyric_manager.rs \
               search_lyrics (search_request_for, search_request, search_provider)",
        date: "2026-10-19",
        divergences: &[],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Audio/{itemId}/RemoteSearch/Lyrics/{lyricId}",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "DownloadRemoteLyrics",
        ferrofin: "handlers/lyrics.rs download_remote_lyrics (queue_high_priority_refresh) → \
               ferrofin-core lyric_manager.rs download_lyrics (fetch_remote, save_targets, \
               try_save_to_files)",
        date: "2026-10-19",
        divergences: &[
            "A lyricId of only whitespace is a 404 (no provider owns it); upstream's \
         ArgumentException.ThrowIfNullOrWhiteSpace makes it a 500. Jellyfin bug not ported.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Providers/Lyrics/{lyricId}",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "GetRemoteLyrics",
        ferrofin: "handlers/lyrics.rs get_remote_lyrics → ferrofin-core lyric_manager.rs \
               get_remote_lyrics (fetch_remote, parse_by_format)",
        date: "2026-10-19",
        divergences: &[],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Localization/Countries",
        upstream_file: "Jellyfin.Api/Controllers/LocalizationController.cs",
        upstream_method: "GetCountries",
        ferrofin: "handlers/localization.rs get_countries → ferrofin-core localization_manager.rs \
               get_countries (COUNTRIES, data/countries.json byte-equal to v12.2)",
        date: "2026-10-20",
        divergences: &[],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Branding/Splashscreen",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "UploadCustomSplashscreen",
        ferrofin: "handlers/branding.rs upload_splashscreen → handlers/image_upload.rs \
               (try_get_image_extension_from_content_type → ferrofin-model mime_types.rs \
               to_extension, decode_base64)",
        date: "2026-10-20",
        divergences: &[
            "The body is decoded in full before anything is written, and the location is \
         recorded only after the file is: a body the decoder rejects is a 400 that changes \
         nothing. Upstream records the new SplashscreenLocation first and decodes while \
         copying, so invalid base64 fails mid-copy with a 500 (FormatException), and any \
         write failure leaves the recorded location pointing at a missing or truncated \
         file. FromBase64Transform also silently drops a trailing partial group (fewer \
         than four characters): an unpadded or short padded final group (`aGk`, `aG=`) \
         loses its last bytes, and a third `=` or a partial group after the padding is \
         ignored. Ferrofin decodes the final group in full and refuses the other two with \
         a 400. Jellyfin bugs not ported.",
            "The body is read as UTF-8 text before the Content-Type is checked, so a body \
         that is not UTF-8 is the extractor's 400 whatever its type; upstream checks the \
         type first and fails such a body with a 500 while decoding. Jellyfin bug not \
         ported.",
            "The body is capped by axum's implicit 2 MiB limit, where Kestrel accepts 30 MB. \
         Fix path: gl-24.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Users/AuthenticateWithQuickConnect",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "AuthenticateWithQuickConnect",
        ferrofin: "handlers/users.rs authenticate_with_quick_connect, authentication_result → \
               ferrofin-core quick_connect_manager.rs get_authorized_request (the result \
               authorize_request stored)",
        date: "2026-10-20",
        divergences: &[
            "The User member is built at the exchange, from the user as it is then, where \
         upstream returns the UserDto it snapshotted when the code was authorized, up to \
         10 minutes stale (a user deleted meanwhile gets the bare id/name DTO); the \
         session, token and ServerId are the stored ones on both. Jellyfin bug not \
         ported.",
            "The 401 (Quick Connect disabled) and 404 (unknown or expired secret) bodies are \
         Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/QuickConnect/Initiate",
        upstream_file: "Jellyfin.Api/Controllers/QuickConnectController.cs",
        upstream_method: "InitiateQuickConnect",
        ferrofin: "handlers/quick_connect.rs initiate → ferrofin-core quick_connect_manager.rs \
               try_connect (make_room, draw_free_code)",
        date: "2026-10-20",
        divergences: &[
            "A drawn code that a pending request already holds is drawn again; upstream \
         TryConnect's `_currentRequests[code] = result` overwrites the other device's \
         request, so authorizing that code would pair the wrong device. Jellyfin bug not \
         ported. Once all 900,000 codes are pending, Initiate fails with 500 instead.",
            "Each remote IP holds at most FERROFIN_QUICK_CONNECT_PENDING_PER_IP (default 10) \
         unauthorized requests, and the next Initiate from that IP drops its oldest, whose \
         poll then gets 404 as an expired one does; authorized requests are exempt. \
         Upstream has no cap, so the anonymous endpoint can grow the map toward the whole \
         code space.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/LiveTv/ListingProviders",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "AddListingProvider",
        ferrofin: "handlers/live_tv.rs add_listing_provider → ferrofin-livetv manager.rs \
               save_listing_provider (ListingsBackend::of, validate_xmltv, \
               store_listing_provider) → schedules_direct/mod.rs SchedulesDirect::validate \
               (ListingsManager.SaveListingProvider, XmlTvListingsProvider.Validate, \
               SchedulesDirect.Validate)",
        date: "2026-10-21",
        divergences: &[
            "An XMLTV provider without a Path is a 400; upstream's Validate dereferences \
         the null path and answers a NullReferenceException 500. Jellyfin bug not ported.",
            "Schedules Direct's /token body is serialized, so a username with a quote or a \
         backslash stays valid JSON; upstream concatenates it into the body raw. Jellyfin \
         bug not ported.",
            "SchedulesDirect.GetTokenInternal logs the session token at info; Ferrofin never \
         logs the password, its hash or the token. Jellyfin bug not ported.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/LiveTv/ListingProviders/Lineups",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetLineups",
        ferrofin: "handlers/live_tv.rs get_lineups → ferrofin-livetv manager.rs get_lineups \
               (ListingsBackend::of, provider_channels) → schedules_direct/mod.rs \
               SchedulesDirect::get_lineups (ListingsManager.GetLineups, \
               XmlTvListingsProvider.GetLineups, SchedulesDirect.GetHeadends)",
        date: "2026-10-21",
        divergences: &[
            "A blank id is a 404 whatever the type; upstream calls \
         GetProvider(type).GetLineups(null, …), which is a 404 for an unknown type but \
         dereferences the null provider (500) for xmltv and SchedulesDirect. Jellyfin bug \
         not ported.",
            "The country and postal code are percent-encoded into Schedules Direct's \
         headends query; upstream concatenates them raw, so a value holding `&` or `#` \
         changes the query. Jellyfin bug not ported.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/{itemId}/ContentType",
        upstream_file: "Jellyfin.Api/Controllers/ItemUpdateController.cs",
        upstream_method: "UpdateItemContentType",
        ferrofin: "handlers/item_update.rs update_item_content_type → ferrofin-core \
               library_manager.rs containing_folder_path (BaseItem/Video/Playlist and the \
               by-name ContainingFolderPath overrides) → ferrofin-model \
               content_type_overrides.rs with_content_type_override",
        date: "2026-10-21",
        divergences: &[
            "The 404 (no such item, or one the caller may not see) body is Ferrofin's error \
         shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/System/Configuration/{key}",
        upstream_file: "Jellyfin.Api/Controllers/ConfigurationController.cs",
        upstream_method: "GetNamedConfiguration",
        ferrofin: "handlers/config.rs get_named_configuration → ferrofin-core \
               configuration_manager.rs get_named_configuration and get_encoding_options \
               (read_named; BaseConfigurationManager.GetConfiguration and \
               LoadConfiguration, the seven ConfigurationStore registrations) → \
               encoding_response for encoding, the Live TV manager's tuner and provider rows \
               for livetv",
        date: "2026-10-23",
        divergences: &[
            "The stored document is reread on every call instead of cached from the first load \
         until a save, so a hand-edited file applies without a restart, and a document that \
         does not parse is logged at warn once per change of the file where upstream logs it \
         at error once.",
            "A stored document that exists but cannot be read is a 500, and so is a save of \
         encoding (the one store with a validator) over it. LoadConfiguration catches every \
         exception but IOException, so upstream serves the defaults for a permission-denied \
         read (UnauthorizedAccessException, logged at error) and for a directory in the \
         file's place (File.Exists is false), and only other I/O errors fail. Jellyfin quirk \
         not ported: serving the defaults lets the next save replace a document nobody saw.",
            "database always reads as Jellyfin-SQLite without locking, the document Jellyfin \
         writes on its first start: Ferrofin has no other database provider and refuses to \
         save another (owner decision 2026-10-23).",
            "livetv's TunerHosts and ListingProviders are the Live TV manager's rows, kept in \
         the database rather than in the document; when they cannot be read the lists are \
         empty and the failure is logged.",
            "For a caller who is not an administrator, livetv's TunerHosts[].Url and \
         ListingProviders[].Password are blanked; upstream's plain [Authorize] serves those \
         credentials to every signed-in user (owner decision 2026-10-23).",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Items/Counts",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetItemCounts",
        ferrofin: "handlers/items.rs get_item_counts (resolve_user_opt, the \
               RequestHelpers.GetUserId port) → ferrofin-core library_manager.rs \
               get_item_counts → item_count_service.rs get_item_counts \
               (scope_to_user_libraries, the LibraryManager.AddUserToQuery port; \
               ItemCountService.GetItemCounts as QueryShape::TypeCounts and \
               counts_from_type_map)",
        date: "2026-10-24",
        divergences: &[
            "isFavorite with no user (an API key without userId, or a userId naming no \
         account) is ignored and the totals are unfiltered (200); upstream's TranslateQuery \
         dereferences the null User for the favourites subquery and fails (500). Jellyfin bug \
         not ported.",
        ],
        refs: &[],
    },
];

/// What `/Artists` and `/Artists/AlbumArtists` do differently from v12's
/// `ArtistsController` + `BaseItemRepository.GetItemValues`.
const BY_NAME_DIVERGENCES_ARTISTS: &[&str] = &[
    "`studios` names resolve without writing: v12's LibraryManager.GetStudio CREATES a row \
     for an unknown name on this GET; Ferrofin's lookup creates none and gives the name a \
     no-match id (by_name::resolve_by_name_ids), the same empty answer.",
    "A `parentId` that is not a folder: v12 puts it in ItemIds (the value is carried by \
     that one item); Ferrofin scopes by AncestorIds in every case, so a non-folder parent \
     answers with nothing.",
    UNKNOWN_PARENT_ID_DIVERGENCE,
    "NameStartsWith matches COALESCE(SortName, Name) where v12 matches SortName alone \
     (Ferrofin leaves by-name SortName NULL) — deliberate, recorded in \
     append_by_name_filters.",
    "A `userId` naming no user: v12's GetUserById returns null and the query runs with no \
     user — unscoped, every library on the server; Ferrofin answers 404. Jellyfin bug not \
     ported.",
    "With `filters=IsPlayed`/`IsUnplayed` and counts requested, a listed row whose carriers \
     are all on the other side of the played filter gets the ten count fields as 0 where v12 \
     omits them (its `ItemCounts?` is null for a name the count scope never saw). \
     Un-defer path: make `ItemWithCounts.counts` an `Option` in ferrofin-traits.",
    "`minCommunityRating`, `person`, `personIds`, `personTypes` are accepted and ignored, \
     exactly as v12 — GetItemValues copies none of them onto either filter.",
];

/// What `/Genres`, `/MusicGenres` and `/Studios` do differently from their v12
/// controllers + `GetItemValues`. These controllers take a subset of the
/// Artists parameters; Ferrofin's shared query struct accepts the superset.
const BY_NAME_DIVERGENCES_GENRES_STUDIOS: &[&str] = &[
    "A `userId` naming no user: v12's GetUserById returns null and the query runs with no \
     user — unscoped, every library on the server; Ferrofin answers 404. Jellyfin bug not \
     ported.",
    "The shared query struct honors `filters`, `mediaTypes`, `genres`, `tags`, `years`, \
     `officialRatings`, `genreIds`, `studioIds` — and, for /Studios, `sortBy`/`sortOrder` — \
     which these v12 controllers do not declare (ASP.NET ignores them). A client sending \
     one gets a narrowed or re-ordered list here and the full SortName-ordered list there.",
    "A `parentId` that is not a folder: v12 puts it in ItemIds; Ferrofin scopes by \
     AncestorIds in every case, so a non-folder parent answers with nothing.",
    UNKNOWN_PARENT_ID_DIVERGENCE,
    "NameStartsWith matches COALESCE(SortName, Name) where v12 matches SortName alone \
     (Ferrofin leaves by-name SortName NULL) — deliberate, recorded in \
     append_by_name_filters.",
];

/// Included in the divergence lists of the five by-name VERIFIED rows; the
/// text also names `/Years` and `/Items/Filters`, which have no row to carry it.
const UNKNOWN_PARENT_ID_DIVERGENCE: &str = "A `parentId` naming no item: v12's GetParentItem \
     throws ArgumentException (400 via ExceptionMiddleware); Ferrofin answers 200 with an \
     empty list. The same holds on /Genres, /MusicGenres, /Studios, /Artists, \
     /Artists/AlbumArtists, /Years, and /Items/Filters unless includeItemTypes is a lone \
     Trailer/Program (there the answer is empty filters). Fix path: answer \
     ApiError::BadRequest(\"Invalid parent id: …\") as items.rs does.";

/// What `/UserItems/Resume` — and its legacy twin `/Users/{userId}/Items/Resume`,
/// the same handler with the user from the route, which is hidden from the
/// contract (`[ApiExplorerSettings(IgnoreApi = true)]`) and so has no row —
/// does differently from v12's `ItemsController.GetResumeItems` +
/// `TranslateQuery`'s `IsResumable` block.
const RESUME_DIVERGENCES: &[&str] = &[
    "BuildHasDescendantFilter's LinkedChildren arm is not emitted: the only folder kinds \
     the resumable rule applies to are Series and Season (_resumableFolderKinds), which \
     never have linked children, so the arm can never match.",
    "A resumable Series or Season row's UnplayedItemCount / PlayedPercentage / Played \
     count each leaf's own played row; v12.2's ItemCountService counts a leaf played \
     through an alternate version (DescendantQueryHelper.IsPlayedBy). Fix path: the \
     alternate-version arm in item_count_service's played statements (gl-441).",
    "Resume positions that Ferrofin builds before gl-46 saved for songs and for Sample, \
     ThemeVideo and Trailer extras (kinds v12 never lets resume) keep those items listed \
     until they are played again; new plays no longer save one. Fix path: gl-717.",
];

/// Compile-time `str` equality (`==` on `&str` isn't const).
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Every [`EXTENSION_ROUTES`] entry must name a real route — a moved or
/// renamed route fails the BUILD, so the ownership manifest can't drift.
const fn extension_routes_all_real() -> bool {
    let mut i = 0;
    while i < EXTENSION_ROUTES.len() {
        let (m, p, _ext) = EXTENSION_ROUTES[i];
        let mut found = false;
        let mut j = 0;
        while j < REAL_ROUTES.len() {
            let (rm, rp) = REAL_ROUTES[j];
            if str_eq(m, rm) && str_eq(p, rp) {
                found = true;
                break;
            }
            j += 1;
        }
        if !found {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = assert!(
    extension_routes_all_real(),
    "EXTENSION_ROUTES contains an entry that is not in REAL_ROUTES"
);

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
