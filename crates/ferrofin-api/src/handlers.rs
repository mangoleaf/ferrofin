//! Real ported handlers for the contract routes.
//!
//! Each submodule mirrors one `Jellyfin.Api` controller and holds axum handlers
//! that call the [`AppState`] manager traits, project
//! results through [`DtoService`](ferrofin_traits::dto::DtoService), and return
//! the wire DTOs from `ferrofin-model`. Every operation in the vendored contract
//! is served by a real handler, listed in [`REAL_ROUTES`], and [`register`]
//! mounts them. A contract route missing from that list would get the shared
//! `not_implemented` `501` stub, never a `404`.
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
/// directory service: `Default` modes, nothing replaced), and refreshes as
/// [`LibraryManager::queue_item_refresh`] says: a file item through the
/// library scan of its path, whose probe re-runs because its sidecar set
/// changed (`ProbeProvider.HasChanged`). An item no longer stored is not
/// refreshed, as the refresh queue skips it.
///
/// [`MetadataRefreshOptions::default`]: ferrofin_traits::providers::MetadataRefreshOptions::default
/// [`LibraryManager::queue_item_refresh`]: ferrofin_traits::library::LibraryManager::queue_item_refresh
pub(crate) async fn queue_high_priority_refresh(
    state: &AppState,
    item_id: Uuid,
) -> Result<(), ApiError> {
    use ferrofin_traits::providers::MetadataRefreshOptions;
    let Some(item) = state.library.get_item_by_id(item_id).await? else {
        return Ok(());
    };
    state
        .library
        .queue_item_refresh(&item, &MetadataRefreshOptions::default())
        .await?;
    Ok(())
}

/// Runs an item's default metadata refresh and returns once it has run —
/// `item.RefreshMetadata(CancellationToken.None)`, which a handler awaits
/// rather than queues ([`LibraryManager::refresh_item_metadata`]), on a task
/// of its own so a client that hangs up does not withdraw it. An item no
/// longer stored is not refreshed.
///
/// [`LibraryManager::refresh_item_metadata`]: ferrofin_traits::library::LibraryManager::refresh_item_metadata
pub(crate) async fn run_item_refresh(state: &AppState, item_id: Uuid) -> Result<(), ApiError> {
    use ferrofin_traits::providers::MetadataRefreshOptions;
    let Some(item) = state.library.get_item_by_id(item_id).await? else {
        return Ok(());
    };
    let library = std::sync::Arc::clone(&state.library);
    let ran = run_detached(async move {
        library
            .refresh_item_metadata(&item, &MetadataRefreshOptions::default())
            .await
    })
    .await?;
    if !ran {
        return Err(ApiError::ServiceUnavailable(format!(
            "the refresh of item {item_id} was stopped before it finished"
        )));
    }
    Ok(())
}

/// Runs work a handler awaits on a task of its own, so a client that hangs up
/// does not stop it: upstream passes `CancellationToken.None`, or no token,
/// and never reads `RequestAborted`. The task keeps the request's span (the
/// request awaits it, LOGGING.md §4) and its culture, which a spawned task
/// does not inherit (.NET's `ExecutionContext` flows it). Once the client has
/// gone, no answer carries the work's error, so the task logs it as the
/// error boundary would have (LOGGING.md §2).
pub(crate) async fn run_detached<T, E>(
    work: impl std::future::Future<Output = Result<T, E>> + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    E: Into<ApiError> + Send + 'static,
{
    let (answer, answered) = tokio::sync::oneshot::channel();
    let culture = ferrofin_util::current_culture::capture();
    tokio::spawn(tracing::Instrument::in_current_span(
        ferrofin_util::current_culture::scope(culture, async move {
            if let Err(Err(error)) = answer.send(work.await.map_err(Into::into)) {
                // The client has gone: building its answer logs a 5xx once, as
                // the error boundary does for a client still connected.
                drop(axum::response::IntoResponse::into_response(error));
            }
        }),
    ));
    // No answer: the work panicked, which the panic hook logged (LOGGING.md §7),
    // or the shutdown dropped it.
    answered.await.unwrap_or_else(|_| {
        Err(ApiError::Service(
            ferrofin_traits::error::ServiceError::backend(
                "the request's task stopped before it answered",
            ),
        ))
    })
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
pub mod ferrofin_settings;
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
pub mod ldap;
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
mod query_date_time;
pub(crate) mod query_parse;
pub mod quick_connect;
pub mod remote_images;
pub mod scheduled_tasks;
pub mod search;
pub mod session;
pub(crate) mod session_ctx;
pub mod similar;
pub mod sso;
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
    // Progressive streams: `Static=true` is served from the source it names;
    // anything else, and a disc folder, is transcoded
    // (`streaming::static_or_transcode`). The HLS routes follow.
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
    // Branding reads (the Splashscreen GET/POST/DELETE rows are further down).
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
    // List, fetch-by-id, and manual run-now. Cancel and the trigger config are
    // listed further down.
    ("get", "/ScheduledTasks"),
    ("get", "/ScheduledTasks/{taskId}"),
    ("post", "/ScheduledTasks/Running/{taskId}"),
    // Batch 16 — similar items, image writes, task cancel and triggers, the
    // metadata editor and view grouping options.
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
    // (`TmdbController`), answered from fixed TMDb values.
    // TODO(parity, gl-1508): open work item, not an accepted divergence: port
    // upstream's live `GetConfigAsync` fetch.
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
    // (see `handlers::library_structure` / `handlers::library`).
    // `AvailableOptions` lists the registered metadata, image, subtitle, lyric
    // and media-segment providers.
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
    // Plugins — the plugin-manager surface (`PluginsController` +
    // `PackageController`) over the plugin registry: compiled-in (Tier 1a)
    // plugins and the WASM (Tier 1b) plugins the host loaded. Reads,
    // enable/disable, config, the repository list, and WASM install and
    // uninstall are real; a compiled-in plugin refuses both. See
    // `handlers::plugins`.
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
    // Live TV (`LiveTvController`): reads, tuner and listing-provider
    // configuration, and the DVR timers, over the `LiveTvManager` (see
    // `handlers::live_tv`).
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

/// `(method, path, extension-id)` for every route a compiled-in extension
/// registers that is **not a contract operation**: a ported plugin's own
/// controller, which the vendored spec does not describe.
///
/// They are in neither [`REAL_ROUTES`] nor [`EXTENSION_ROUTES`] (contract
/// operations only) and carry no [`VERIFIED`] row; each ported plugin's
/// comparison with its upstream is recorded in `docs/PLUGINS_UPSTREAM.md`, and
/// Ferrofin's own Single Sign-On plugin has none to compare with. Like every
/// ASP.NET route they are matched ignoring case
/// ([`canonicalize_path`](crate::routes::canonicalize_path) reads this list):
/// a plugin's settings page calls them as it spells them.
pub const PLUGIN_ROUTES: &[(&str, &str, &str)] = &[
    // LDAP-Auth plugin (see handlers::ldap).
    ("post", "/Ldap/TestServerBind", "ldap-auth"),
    ("post", "/Ldap/TestLdapFilters", "ldap-auth"),
    ("post", "/Ldap/LdapUserSearch", "ldap-auth"),
    // Single Sign-On plugin (see handlers::sso): Ferrofin's own, with no
    // upstream to compare with (D15).
    ("get", "/sso/admin/oidc/providers", "sso"),
    ("put", "/sso/admin/oidc/providers/{provider}", "sso"),
    ("delete", "/sso/admin/oidc/providers/{provider}", "sso"),
    ("get", "/sso/oidc/{provider}/start", "sso"),
    ("get", "/sso/oidc/{provider}/callback", "sso"),
];

/// Every path a handler registers that is neither a contract route nor in
/// [`PLUGIN_ROUTES`]: the obsolete routes upstream leaves out of its OpenAPI
/// document (`/Users/{userId}/Items`, …), extension extras, and Ferrofin's own.
///
/// ASP.NET matches every route ignoring case, so
/// [`canonicalize_path`](crate::routes::canonicalize_path) reads this list
/// too; a test in `routes.rs` keeps it in step with the handlers' route calls.
pub const UNLISTED_ROUTES: &[&str] = &[
    "/Ferrofin/Settings",
    "/Intros/DisabledEpisodes/Update",
    "/Intros/DisabledEpisodes/{SeasonId}",
    "/Intros/ExcludedTimestamps/Clear",
    "/Intros/Show/{MovieId}",
    "/Items/{itemId}/Collections",
    "/MediaSegments/Provider/{providerId}",
    "/Plugins/{pluginId}/web/{*path}",
    "/Users/{userId}/Authenticate",
    "/Users/{userId}/Configuration",
    "/Users/{userId}/FavoriteItems/{itemId}",
    "/Users/{userId}/GroupingOptions",
    "/Users/{userId}/Images/{imageType}",
    "/Users/{userId}/Images/{imageType}/{imageIndex}",
    "/Users/{userId}/Items",
    "/Users/{userId}/Items/Latest",
    "/Users/{userId}/Items/Resume",
    "/Users/{userId}/Items/Root",
    "/Users/{userId}/Items/{itemId}",
    "/Users/{userId}/Items/{itemId}/Intros",
    "/Users/{userId}/Items/{itemId}/LocalTrailers",
    "/Users/{userId}/Items/{itemId}/Rating",
    "/Users/{userId}/Items/{itemId}/SpecialFeatures",
    "/Users/{userId}/Items/{itemId}/UserData",
    "/Users/{userId}/Password",
    "/Users/{userId}/PlayedItems/{itemId}",
    "/Users/{userId}/PlayingItems/{itemId}",
    "/Users/{userId}/PlayingItems/{itemId}/Progress",
    "/Users/{userId}/Suggestions",
    "/Users/{userId}/Views",
    "/embywebsocket",
    "/socket",
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
/// (2026-09-02). The pin is the **latest Jellyfin release when a release branch
/// is cut**, never a release candidate (2026-10-16); the branch keeps it until
/// its release, and the next release branch moves to the newest release
/// (2026-10-25). To move it, move this tag and the commit, set
/// [`UPSTREAM_PIN_DATE`] to that day, and list every row not already listed in
/// [`VERIFIED_AGAINST_EARLIER_PIN`] (or, for a subsystem row,
/// [`VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN`]) under the old tag.
/// The vendored OpenAPI contract stays 10.11.8: that is the API clients speak,
/// even though Jellyfin 12 dropped operations from it.
pub const UPSTREAM_TAG: &str = "v12.4";
/// The commit `UPSTREAM_TAG` resolves to in `github.com/jellyfin/jellyfin`.
pub const UPSTREAM_COMMIT: &str = "83d0d0e7f7";
/// The day [`UPSTREAM_TAG`] became the pin, `YYYY-MM-DD`; move it with the tag.
/// A row dated on or after it was compared against [`UPSTREAM_TAG`], so it can
/// never be listed in [`VERIFIED_AGAINST_EARLIER_PIN`] or
/// [`VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN`]; a row dated before it was
/// compared against an earlier release, so it must be listed there.
pub const UPSTREAM_PIN_DATE: &str = "2026-10-28";

/// [`VERIFIED`] rows last compared against an earlier pin, as
/// `(method, path, tag compared against)`. They are completed verification work,
/// but not against [`UPSTREAM_TAG`], and the parity line counts them apart.
/// Re-compare the row against [`UPSTREAM_TAG`], update its `date`, and remove it
/// from this list. `v12.2` is commit `ca0f16eb7c`.
pub const VERIFIED_AGAINST_EARLIER_PIN: &[(&str, &str, &str)] = &[
    // The v12.4 re-check found a gap that v12.2 has too (the C# under these
    // rows is unchanged); each leaves the list with its fix.
    // gl-884: a malformed genreIds/studioIds token is a 400, not dropped.
    ("get", "/Artists", "v12.2"),
    ("get", "/Artists/AlbumArtists", "v12.2"),
    // gl-762: livetv lists tuners and listing providers by id, not as added.
    ("get", "/System/Configuration/{key}", "v12.2"),
];

/// [`VERIFIED_SUBSYSTEMS`] rows last compared against an earlier pin, as
/// `(key, tag compared against)` — the same rules as
/// [`VERIFIED_AGAINST_EARLIER_PIN`].
pub const VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN: &[(&str, &str)] = &[];

/// The subsystems a [`VERIFIED_SUBSYSTEMS`] row may cover, by key. Every
/// operation crosses several of them, so one row states their divergences once
/// and operation rows name it in [`Verified::refs`]. `static-file-ranges` is
/// how a file served with range processing on answers `Range` and `If-Range`,
/// shared by direct play, the download and file routes, the HLS segments and
/// a graphical subtitle's file, most of whose operations have no row of their
/// own yet.
/// `websocket-streams` is
/// the `/socket` dashboard streams, which no contract operation describes;
/// `trickplay-extraction` is the ffmpeg run that writes a video's trickplay
/// frames, which the tile operations serve but no response shows;
/// `image-extraction` is the ffmpeg run that writes one image (a chapter
/// image, a screen-grabbed Primary, audio cover art or an embedded image
/// stream), which no response shows either;
/// `trickplay` is which sets of trickplay images are made, kept and replaced,
/// shared by the item refresh, the library scan and the Generate Trickplay
/// Images task, and the order a media source's widths are listed in
/// `BaseItemDto.Trickplay`;
/// `scan-failure-isolation` is what the library scan does about a row or a
/// write that fails, which no operation's response shows;
/// `progressive-transcode` is who may reach ffmpeg through the stream routes,
/// whose operations have no row of their own yet, when the progressive body
/// they answer with ends, and how many jobs ffmpeg may run for them and the
/// HLS routes;
/// `stream-parameter-patterns` is which container, codec and level values the
/// HLS and stream routes refuse before their action runs, whose operations
/// have no row of their own yet;
/// `transcode-arguments` is the ffmpeg command line of a job on the stream
/// routes and the HLS routes, in the parts its row names;
/// `video-stream-copy` is whether a transcode job copies the source video and
/// what dynamic HDR metadata the copy strips, which the HLS and stream routes
/// share; `recording-postprocessing` is the executable run after a retained
/// DVR capture, which no HTTP response describes; `recording-images` is the
/// guide artwork a starting capture copies beside its recording, which none
/// describes either; `recording-keep-up-to` is
/// which of a series timer's recordings a starting capture deletes past its
/// "Keep up to", and `direct-recording` how the DVR copies an unshared tuner
/// URL and how that copy's end settles the timer (completed, or failed and
/// retried), which no HTTP response describes either. `metadata-refresh` is
/// which metadata and image providers an item's refresh runs, in which order
/// and with what lookup, the Identify case first, shared by Identify → Apply,
/// the item refresh and the library scan, and `metadata-providers` what
/// TheMovieDb's movie, series, season and episode providers write onto an item
/// from TMDb's answer. `guide-programme-items` is what the guide refresh
/// stores on each programme item and its external ids, which every programme
/// operation reads.
pub const SUBSYSTEM_KEYS: &[&str] = &[
    "query-translation",
    "dto-fields",
    "user-data",
    "access-filtering",
    "images",
    "static-file-ranges",
    "paging-sort",
    "websocket-streams",
    "scan-failure-isolation",
    "progressive-transcode",
    "stream-parameter-patterns",
    "transcode-arguments",
    "trickplay-extraction",
    "image-extraction",
    "trickplay",
    "video-stream-copy",
    "recording-postprocessing",
    "recording-images",
    "recording-keep-up-to",
    "direct-recording",
    "metadata-refresh",
    "metadata-providers",
    "guide-programme-items",
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
pub const VERIFIED_SUBSYSTEMS: &[VerifiedSubsystem] = &[
    VerifiedSubsystem {
        key: "guide-programme-items",
        upstream_file: "src/Jellyfin.LiveTv/Guide/GuideManager.cs",
        upstream_method: "GetProgram",
        ferrofin: "What the guide refresh stores on each programme item and its external \
                   ids. ferrofin-livetv manager.rs insert_programs and \
                   insert_schedules_direct_programs keep the listing's Etag (xmltv_etag over \
                   xmltv_etag.rs xmltv_program_etag, a port of XmlTvProgramEtag.TryCreate; \
                   Schedules Direct's md5), Audio, IsHD, OriginalAirDate and Zap2It series id \
                   in FerrofinLiveTvPrograms; sync_channel_program_items saves the items \
                   (projection.rs program_entity for a new airing, program_item_update for a \
                   stored one) and replaces their ids (program_provider_ids, \
                   program_provider_ids_replaced, replace_provider_ids_for_items). Compared \
                   with GuideManager.GetProgram, XmlTvListingsProvider.GetProgramInfo and \
                   GetProgramInfoWithEtag and src/Jellyfin.LiveTv/Listings/XmlTvProgramEtag.cs \
                   at the pinned v12.4 tag, and the etag bytes with those a Jellyfin 12.4 \
                   server stored for the same guide. The XMLTV reader keeps a guide's text \
                   as Jellyfin.XmlTv reads it (xmltv.rs apply_end, end_programme_run, \
                   push_text; gl-1631), checked against that server on padded, empty, blank, \
                   CRLF and date text, and its times and dates through one port of \
                   XmlTvReader.ParseDate (parse_xmltv_time, copyright_year; gl-1646) that \
                   upstream's XmlTvReaderDateTimeTests pin, with PopulateHeader's \
                   MinValue for a missing time in the guide window (manager.rs \
                   in_guide_window) and the previously-shown check.",
        date: "2026-11-11",
        divergences: &[
            "Jellyfin.XmlTv drops the element right after an <image> or <url> when no \
             whitespace separates them: ProcessImageNode reads past the end tag and its caller \
             skips one more node. Ferrofin reads that element, so on such a compact guide the \
             airing's rating, artwork or other field, and its etag, differ from Jellyfin's. \
             Jellyfin bug not ported.",
            "An airing whose start is missing or does not parse (and whose stop is in the \
             guide window) is kept with no start date and one id per channel and title; \
             upstream's PopulateHeader takes DateTimeOffset.MinValue, year 1, which gives \
             every such airing of a channel the same programme id. Jellyfin bug not ported.",
            "A programme's image URL padded with whitespace is stored trimmed, so the \
             artwork loads; upstream's UpdateImage stores the padded path, which does not. \
             Jellyfin bug not ported.",
            "A programme is saved only when a field GetProgram assigns changed. Upstream skips \
             only an XMLTV airing whose etag equals the stored one; every other airing (each \
             Schedules Direct one, whose md5 is compared after its ids were replaced, and an \
             XMLTV one with no etag) is saved on every refresh, which moves its \
             DateLastSaved and the DTO Etag derived from it. Fix path: gl-1628 (owner \
             decision).",
        ],
    },
    VerifiedSubsystem {
        key: "paging-sort",
        upstream_file: "Jellyfin.Api/Helpers/RequestHelpers.cs",
        upstream_method: "GetOrderBy",
        ferrofin: "How sortBy and sortOrder pair into an order: handlers/query_parse.rs \
                   get_order_by (no keys, no pairs; a key takes the order at its index, the \
                   keys past the last order the first order, else Ascending) and \
                   parse_order_by (CommaDelimitedCollectionModelBinder's single-value arm, \
                   parse_csv_enums_lenient: split on commas with empty entries dropped, each \
                   trimmed, a name in any case or a C# value, a token that does not convert \
                   dropped), for /Items, /Trailers, /Years, /Artists, /Artists/AlbumArtists, \
                   /Genres, /MusicGenres, /Items/{itemId}/ThemeSongs, ThemeVideos and \
                   ThemeMedia and GET /LiveTv/Programs; get_order_by alone after POST \
                   /LiveTv/Programs' JSON body and after activity_log.rs's strict binder for \
                   /System/ActivityLog/Entries. Compared with GetOrderBy, \
                   RequestHelpersTests.GetOrderBy_Success, CommaDelimitedCollectionModelBinder \
                   and the GetOrderBy calls in ItemsController (TrailersController through it), \
                   YearsController, ArtistsController, GenresController, \
                   MusicGenresController, LibraryController, LiveTvController and \
                   ActivityLogController at the pinned v12.4 tag. ChannelsController's call has \
                   no counterpart: with no channel provider ported, GET \
                   /Channels/{channelId}/Items answers 400 whatever the sort, as upstream does \
                   after pairing. A key repeated in the query is the \
                   shared query extractor's (crate::query), and paging (startIndex, limit) is \
                   not part of this row. Of what each key sorts by, only DatePlayed is part of \
                   it; the other keys (PlayCount, IsPlayed, IsUnplayed and IsFavoriteOrLiked \
                   among them) are not. DatePlayed sorts by the newest LastPlayedDate of the \
                   item and its alternate versions over the query's user's UserData rows, or \
                   every user's when there is no user, for \
                   /Items and the other item queries (ferrofin-core translate_query.rs \
                   push_order_expression), GET /LiveTv/Channels (ferrofin-livetv manager.rs \
                   get_channels over guide_repository.rs channel_user_data or \
                   channel_last_played_by_anyone, then projection.rs sort_channel_rows) and \
                   the guide (manager.rs push_program_sort_key). A channel and a programme \
                   have no alternate versions, so their key is their own rows' alone. \
                   Compared with OrderMapper.MapOrderByField's DatePlayed arm at the pinned \
                   v12.4 tag.",
        date: "2026-11-15",
        divergences: &[
            "A sortOrder with more entries than a non-empty sortBy has keys has the extra \
             entries ignored; upstream's GetOrderBy indexes past the sortBy array, which \
             throws ArgumentOutOfRangeException, and answers 400. Jellyfin bug not ported.",
        ],
    },
    VerifiedSubsystem {
        key: "static-file-ranges",
        upstream_file: "Jellyfin.Api/Helpers/FileStreamResponseHelpers.cs",
        upstream_method: "GetStaticFileResult",
        ferrofin: "Which bytes, status and Content-Range a Range and an If-Range get from a \
                   file served with range processing on: handlers/streaming.rs \
                   serve_static_file (one_byte_range, if_range_valid, complete_length, then \
                   tower-http's ServeFile through serve_file), behind direct play (the /Videos \
                   and /Audio stream routes and universal audio), /Items/{itemId}/Download and \
                   /File, the HLS segment routes and a graphical subtitle's file. Compared with \
                   GetStaticFileResult (VideosController, AudioHelper, HlsSegmentController, \
                   DynamicHlsController) and the PhysicalFile(..., true) calls in \
                   LibraryController (GetFile, GetDownload) and SubtitleController at the \
                   pinned v12.4 tag, and the ASP.NET Core 10 code they reach: \
                   FileResultHelper.SetHeadersAndLog, IfRangeValid and SetRangeHeaders, \
                   RangeHelper.ParseRange and NormalizeRange, and the RangeHeaderValue, \
                   RangeItemHeaderValue and RangeConditionHeaderValue parsers. Several ranges \
                   (a second Range value or any comma) and a value that does not parse (a \
                   reversed range, a part that is not digits, has more than 19 digits or is \
                   past i64::MAX) are ignored and the whole file served; one range takes \
                   spaces and tabs around its parts and leading zeros; an end past the file is \
                   cut to it, a suffix longer than the file is the whole file, a start at or \
                   past the end and a zero suffix are 416 with bytes */length, and no range of \
                   an empty file is satisfiable. If-Range is read only beside a Range that \
                   stands: a date fails once the file's Last-Modified (whole seconds) is later, \
                   a tag fails unless strongly equal to the ETag of the answer (upstream's \
                   answer has none, so it passes every tag; this holds until gl-1823 drops \
                   ServeFile's ETag), a value that parses as neither passes, and a failed one \
                   serves the whole file even where the range would be 416; HEAD answers the \
                   same with no body. The other validators and preconditions (the ETag itself, \
                   If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since), the date \
                   forms .NET's TryStringToDate reads beyond httpdate's IMF-fixdate, RFC 850 \
                   and asctime (If-Range's included) and a 416's Content-Type are gl-1823's, \
                   and each answer's Content-Type is its route's; they are not part of this \
                   row.",
        date: "2026-11-14",
        divergences: &[
            "A Range in a unit other than bytes is ignored and the whole file served, as RFC \
             9110 section 14.2 requires of a unit the server does not understand; ASP.NET's \
             RangeHelper never checks the unit and serves any as a byte range. ASP.NET bug \
             not ported.",
        ],
    },
    VerifiedSubsystem {
        key: "recording-keep-up-to",
        upstream_file: "src/Jellyfin.LiveTv/Recordings/RecordingsManager.cs",
        upstream_method: "EnforceKeepUpTo",
        ferrofin: "ferrofin-livetv manager.rs start_capture (OnStarted: spawned, as its async \
                   void does not hold the recorder up) -> enforce_keep_up_to: no series timer \
                   id, series folder or stored series timer, or KeepUpTo <= 0, does nothing; \
                   under one lock (_recordingDeleteSemaphore) the series timer's Completed \
                   timers whose file (TimerInfo.RecordingPath, kept on the timer's recording \
                   row) is a file (File.Exists, so not a folder), \
                   OrderByDescending(EndDate).Skip(KeepUpTo - 1), each through \
                   delete_library_item_for_timer (DeleteLibraryItemForTimer: FindByPath(path, \
                   false) and DeleteItem with DeleteFileLocation, else the file; then the \
                   timer, disarmed), errors logged per timer; then FindByPath(seriesPath, \
                   true) and its recursive non-folder, non-virtual files by DateCreated \
                   descending (RecordingLibrary::files_under, IsFileProtocol) that are files, \
                   Skip(KeepUpTo - 1), each DeleteItem with DeleteFileLocation, errors \
                   logged per item. FindByPath matches the stored %AppDataPath% form \
                   (TranslateQuery's GetPathToSave), then the path as given, for a row \
                   Ferrofin stored resolved. A deleted file's recording rows go with it, as a \
                   library delete's forget_recordings. Compared with EnforceKeepUpTo, \
                   DeleteLibraryItemsForTimers, DeleteLibraryItemForTimer, RecordStream's \
                   OnStarted and LibraryManager.FindByPath at the pinned v12.4 tag.",
        date: "2026-11-11",
        divergences: &[
            "A file being recorded is never selected; upstream's pass over the series \
             folder would select one past the limit that was already a library item, which \
             a Ferrofin capture is not until it ends. The library's delete of a non-episode \
             video that is not in a mixed folder still removes that folder, capture included, \
             as upstream's Video.GetDeletePaths does.",
            "Where the library refuses a delete (a protected folder, or a library this \
             version has not fully scanned yet), the recording and its timer stay and the \
             error is logged; upstream's DeleteItem has no such refusal.",
        ],
    },
    VerifiedSubsystem {
        key: "recording-images",
        upstream_file: "src/Jellyfin.LiveTv/Recordings/RecordingsMetadataManager.cs",
        upstream_method: "SaveRecordingImages",
        ferrofin: "ferrofin-livetv manager.rs start_capture (OnStarted's SaveRecordingMetadata: \
                   spawned, as its async void does not hold the recorder up; the NFO when \
                   SaveRecordingNFO is on, then the images when SaveRecordingImages is on, a \
                   failed NFO ending the task as the catch-all does) -> save_recording_images: \
                   the guide programme item the timer's facts came from \
                   (TimerRecordingInfo.program_item_id, set by apply_program_to_recording_info; \
                   none, like upstream's stand-in programme, saves nothing), its image rows -> \
                   recording_metadata.rs recording_images (a series programme: Thumb ?? \
                   Primary; otherwise Primary ?? Thumb, Backdrop, Thumb, Logo, each by \
                   GetImageInfo(type, 0); named by the chosen image's type: Primary and Thumb \
                   {recording stem}-thumb for a series, else poster and landscape, Backdrop \
                   fanart, Logo logo, in the recording's folder) -> a remote image made local \
                   through ProviderManager::cached_remote_image (ConvertImageToLocal's \
                   download), a local one only from the internal metadata directory \
                   (LiveTvPaths.metadata_dir) -> copy_recording_image (File.Copy with overwrite, the local \
                   file's extension); each image's failure logged and the next one tried. \
                   Compared with SaveRecordingMetadata, SaveRecordingImages, \
                   SaveRecordingImage, RecordingsManager.RecordStream's OnStarted and \
                   LibraryManager.ConvertImageToLocal at the pinned v12.4 tag.",
        date: "2026-11-15",
        divergences: &[
            "The image's extension is appended to the saved name; upstream's \
             Path.ChangeExtension cuts the name at its last dot, so an episode recorded as \
             Dr. Who S01E01 saved its thumb as Dr.jpg, and an image file without an \
             extension was saved with a trailing dot. Jellyfin bug not ported.",
            "A remote image is copied from the remote-image cache and its row keeps the URL, \
             where upstream's ConvertImageToLocal rewrites the programme's row to the \
             downloaded file (the cache's reason is on ProviderManager::cached_remote_image); \
             a row none of whose URLs gave an image stays, where removeOnFailure removes it, \
             as the images row records (owner decision 2026-10-31, gl-1095).",
            "A programme's local image row is copied only when its file is in the internal \
             metadata directory (live_tv_image_is_internal), the rule every other read of a \
             Live TV image row follows, since a guide supplied it; upstream copies any local \
             file. The row is skipped and logged, and the next image tried.",
        ],
    },
    VerifiedSubsystem {
        key: "recording-postprocessing",
        upstream_file: "src/Jellyfin.LiveTv/Recordings/RecordingsManager.cs",
        upstream_method: "PostProcessRecording",
        ferrofin: "ferrofin-livetv manager.rs settle_timer (RecordStream: only the retained \
                   capture branch, after persisting Completed, including a failed capture \
                   whose retry window is exhausted) -> recording_postprocess.rs run: read \
                   current LiveTvOptions, skip a blank executable, expand {path} ignoring \
                   case, launch without a shell, await exit, log spawn/argument errors \
                   without changing recording success. Compared with RecordStream and \
                   PostProcessRecording at the pinned v12.4 tag.",
        date: "2026-11-08",
        divergences: &[
            "Arguments use POSIX shell-word quoting without shell execution; the template \
             is split before replacing {path}, so spaces, quotes and metacharacters in \
             recording names remain literal argument contents instead of changing parsing.",
            "Argument values are not logged because a configured template can contain secrets.",
            "After a configured processor exits, refresh the recording again so changed codecs \
             and duration are discovered even when its library has no filesystem watcher.",
        ],
    },
    VerifiedSubsystem {
        key: "direct-recording",
        upstream_file: "src/Jellyfin.LiveTv/IO/DirectRecorder.cs",
        upstream_method: "RecordFromMediaSource",
        ferrofin: "ferrofin-livetv dvr.rs record_direct, the RecordingInput::Url arm only (the \
                   buffer arm, RecordFromDirectStreamProvider, is not covered): copy the tuner \
                   body until the duration or the cancel, which also interrupts a pending read \
                   (StreamHelper.CopyUntilCancelled), wait out a clean end of stream in \
                   CANCEL_POLL_MS steps, flush and return a transport error -> manager.rs \
                   record_stream and settle_timer \
                   (RecordingsManager.RecordStream: a stop settles Completed; any other failure \
                   retries in RETRY_INTERVAL_SECONDS while UtcNow < EndDate and RetryCount < \
                   MAX_RETRY_COUNT, and a non-empty partial file stays). Compared with \
                   RecordFromMediaSource, CopyUntilCancelled and RecordStream at the pinned v12.4 \
                   tag.",
        date: "2026-11-11",
        divergences: &[
            "The tuner gets 30 s to connect, to answer and between two chunks of the body \
             (TUNER_RESPONSE_TIMEOUT_SECONDS): a tuner silent that long fails the attempt, which \
             retries into a new file. Upstream waits HttpClient's 100 s for the answer and has no \
             timeout on the body, so a stalled connection records nothing more until the \
             programme ends. Jellyfin bug not ported.",
            "The tuner URL is opened with the source's RequiredHttpHeaders (an M3U tuner's \
             User-Agent), as playback opens it; upstream's recorder sends none, so a provider \
             that needs them refuses its recordings. Jellyfin bug not ported.",
            "A tuner answering an error status fails the attempt, which retries; upstream never \
             checks the status, records the error body as the programme and settles it \
             Completed. Jellyfin bug not ported.",
        ],
    },
    VerifiedSubsystem {
        key: "scan-failure-isolation",
        upstream_file: "MediaBrowser.Controller/Entities/TV/Episode.cs",
        upstream_method: "FindSeriesName",
        ferrofin: "What a child's refresh in the library scan does when its parent row cannot \
                   be read, and what the walk does when an item fails. ferrofin-core \
                   library_scan.rs run_scan (the item's error is counted in \
                   ScanOutcome::skipped, the walk goes on, a saved row loses its \
                   DateLastRefreshed, a new item with no row takes its descendants along, a \
                   refresh of an item or folder ends with that item's own failure) and \
                   scan_item (the row is saved unstamped, and its stamp written after what \
                   hangs off it, unless a cancel cut that short) \
                   ← parent_row (a failed read counts as \
                   no row, once per pass) ← sync_series_name (FindSeriesName), \
                   sync_season_name, remember_series_folder, locale_parent (GetParents, \
                   GetCollectionFolders, read by resolve_metadata_locale), \
                   update_season_premiere_date (GetEpisodes, whose null series lists no \
                   episodes, and UpdatePremiereDate, which changes nothing without them); \
                   apply_parental_rating_score and \
                   inherit_stored_rating; keep_stored_series_key; series_display_order \
                   (an unread series skips its children's TheTVDB lookups, counting no \
                   failure). Compared with \
                   MediaBrowser.Controller/Entities/TV/Episode.cs (GetLookupInfo), \
                   MediaBrowser.Controller/LibraryTaskScheduler/\
                   LimitedConcurrencyLibraryScheduler.cs (ProcessItem), \
                   MediaBrowser.Controller/Entities/Folder.cs (RunTasks, \
                   RefreshChildMetadata), \
                   MediaBrowser.Controller/Entities/BaseItem.cs (RefreshMetadata, \
                   GetPreferredMetadataLanguage, GetPreferredMetadataCountryCode, GetParents), \
                   MediaBrowser.Providers/Manager/ProviderManager.cs (RefreshItem), \
                   MediaBrowser.Providers/Manager/MetadataService.cs (RefreshMetadata, \
                   UpdatePremiereDate), \
                   MediaBrowser.Controller/Entities/TV/Season.cs (FindSeriesName, GetLookupInfo, \
                   Series, GetEpisodes), \
                   MediaBrowser.Providers/TV/SeasonMetadataService.cs \
                   (EnableUpdatingPremiereDateFromChildren, GetChildrenForMetadataUpdates), \
                   MediaBrowser.Controller/Entities/BaseItem.cs (GetParentalRatingScore) and \
                   MediaBrowser.Providers/TV/EpisodeMetadataService.cs (BeforeSaveInternal)",
        date: "2026-11-15",
        divergences: &[
            "An item whose parent chain cannot be read is scored from its own rating and \
             country, an ancestor's custom rating not counting, else takes the score stored on \
             its parent, and the children of an undecodable series keep the series key stored \
             on its row. Upstream has no such row to fall back from: the read would throw into \
             the per-child catch and the item would not refresh.",
            "A child whose parent row cannot be read takes its metadata language and country \
             as if its parents ended there, from the collection folder, the library or the \
             server, and one whose collection folder row cannot be read from the library or \
             the server, so an override on the unreadable row or above it goes unused and the \
             child is fetched, and stamped, in that locale, where upstream's read of such a \
             row would throw into the per-child catch and the item would not refresh.",
            "An episode, or a season whose refresh reaches its TheTVDB lookups, whose series \
             row cannot be read is refreshed and stamped without TheTVDB's answer, as \
             upstream treats a child with no series (GetLookupInfo sets no \
             SeriesProviderIds, and jellyfin-plugin-tvdb 5c4592f returns an empty result), \
             where upstream's read of such a row would throw into the per-child catch and \
             the item would not refresh.",
            "A season whose series row cannot be read keeps its premiere date and year where \
             its refresh would take them from its episodes, and is stamped, as upstream treats \
             a season with no series (GetEpisodes lists none, and UpdatePremiereDate changes \
             nothing without children), where upstream's read of such a row would throw into \
             the per-child catch and the season would not refresh.",
            "A stored item whose refresh stops after its row was saved (a failed write, a \
             cancel, a crash) is left with no DateLastRefreshed, which the scan clears for the \
             save and writes back last, so the next scan retries it as a first refresh where \
             upstream, which saves the row after the providers and the probe, retries it \
             because the row kept its old DateModified; the rows a save hangs off it by \
             foreign key are why Ferrofin saves first, and the retry is the same but runs \
             every provider.",
            "A new item that fails before its first save has no row until the next scan, \
             where upstream's CreateItems and first-refresh save keep one, so a new folder \
             that fails takes its descendants with it for this scan.",
            "A stored folder that fails leaves its children to refresh, where \
             RefreshChildMetadata does not recurse into a folder that threw, because the \
             flat walk has already planned them and a persistent failure on a season must \
             not hide its new episodes for good.",
            "An episode whose season row is absent or unreadable keeps the season name it has, \
             the planner's localized one, where FindSeasonName answers a literal \"Season N\" or \
             \"Season Unknown\". Fix path: port that branch to sync_season_name for a row that \
             is truly absent, leaving a row that fails to read out of it.",
        ],
    },
    VerifiedSubsystem {
        key: "trickplay-extraction",
        upstream_file: "MediaBrowser.MediaEncoding/Encoder/MediaEncoder.cs",
        upstream_method: "ExtractVideoImagesOnIntervalAccelerated",
        ferrofin: "The ffmpeg run that writes a video's trickplay frames, which the trickplay \
                   tiles are packed from: ferrofin-mediaencoding encoder/trickplay_extractor.rs \
                   extract_trickplay_frames, plan, build_trickplay_args, SwTonemap and run on the \
                   software path, and build_accelerated_trickplay_args with \
                   supports_keyframe_only_decode, display_corrected_height, frame_matrix and \
                   mjpeg_quality on the hardware one, and extract_trickplay_frame_sets, \
                   extract_together and combined_args for several widths of one video, \
                   compared with \
                   ExtractVideoImagesOnIntervalAccelerated and \
                   ExtractVideoImagesOnIntervalInternal and, in \
                   MediaBrowser.Controller/MediaEncoding/EncodingHelper.cs, \
                   GetVideoProcessingFilterParam, GetSwVidFilterChain, IsSwTonemapAvailable, \
                   the software fallbacks of GetNvidiaVidFilterChain, GetVaapiVidFilterChain and \
                   GetIntelVidFilterChain, and GetInputArgument, GetInputVideoHwaccelArgs and \
                   GetMjpegEncoder as the trickplay job calls them. The vendor filter chains \
                   themselves are the transcode ports in encoding_helper/hw and are not part of \
                   this row",
        date: "2026-11-03",
        divergences: &[
            "The fps= sampler is emitted for every stream, where upstream's GetFramerateParam \
             emits none for a stream ffprobe read no frame rate for, or one whose rate is not \
             above the sampling rate, and then writes a frame per keyframe or throws (12.0-rc3, \
             PR 17304); a stream with no frame rate is sampled on its container timestamps \
             with no setpts. A Jellyfin bug, deliberately not ported. The rate is printed as \
             the exact 1000/interval, not upstream's float widened to double.",
            "An interlaced source is deinterlaced at frame rate whatever \"Double the frame \
             rate when deinterlacing\" says, because upstream's setpts would number the \
             doubled frames at the source's rate and stretch the timeline twofold. A Jellyfin \
             bug, deliberately not ported.",
            "On the software path the graph is built in upstream's order but with the \
             dar-aware software scale rather than GetSwScaleFilter, and with no setparams \
             re-tag of an SDR frame and no closing format=yuv420p, so a 4:2:2 or 4:4:4 source \
             keeps its chroma.",
            "On the software path, on an ffmpeg without tonemapx, an HDR frame is not \
             re-tagged BT.709 either: from ffmpeg 5.1 the scale converts its BT.2020 matrix \
             to BT.601, and the frame stays un-tone-mapped, as upstream's does.",
            "On the software path, on ffmpeg 5.1 or later, the frame is converted to the \
             BT.601 matrix a JPEG decoder reads, in the scale, or behind tonemapx for an HDR \
             source, whose output is BT.709. Upstream converts none, so its BT.709 previews \
             shift colour (gl-1129): a Jellyfin bug, deliberately not ported.",
            "With TrickplayOptions full_chroma, a Ferrofin setting that is off by default, the \
             software path's frame is held at 4:4:4 from the scale on, and an HDR source is \
             tone-mapped ahead of the scale, because tonemapx writes 4:2:0 only.",
            "A run that exits cleanly with no frames counts as failed, so a keyframe-only run \
             that writes none retries the standard way. Upstream retries only after a non-zero \
             exit or a hang, and its TrickplayManager then refuses the empty directory.",
            "With trickplay hardware acceleration on, a job with nothing on the GPU, a source \
             it cannot decode and a software MJPEG encoder, takes the software path with its \
             divergences, where upstream runs the same GetSwVidFilterChain through the vendor \
             chain. Neither initialises a device, except upstream with VAAPI or QSV when the \
             server's hardware encoding is on, its MJPEG encoder is available (iHD for VAAPI) \
             and trickplay's switch is off: GetInputVideoHwaccelArgs re-derives the encoder \
             from the server's switch and starts a device nothing uses.",
            "On ffmpeg 5.1 or later, frames a hardware decoder hands the software MJPEG encoder \
             are converted to BT.601 at the end of the graph, from the matrix they hold (BT.709 \
             when tone-mapped, else the probed colour space; a BT.601 or untagged source is not \
             converted), where upstream converts none (gl-1157): a Jellyfin bug, deliberately \
             not ported. Measured on NVENC; the VAAPI and QSV downloads were measured with CPU \
             scalers in their formats, not on Intel or AMD hardware. Frames for a hardware \
             MJPEG encoder are not converted yet, as upstream's are not: open work (gl-1169).",
            "A stream whose aspect ratio is 0:1 or does not parse keeps its stored height, \
             where upstream's height correction, which runs for every job, throws and the item \
             gets no trickplay. For 1:0, which upstream turns into a height of 0 that only a \
             fixed-size hardware scaler reads, and which then throws there too, the hardware \
             path keeps the stored height. Jellyfin bugs, deliberately not ported.",
            "The caller owns the frame directory, which the extractor clears before a retry, \
             where upstream extracts each run into a fresh temporary directory.",
            "Several widths of one video are made by one ffmpeg run, which decodes the video \
             once, where RefreshTrickplayDataAsync calls ExtractVideoImagesOnIntervalAccelerated \
             once per width (gl-1228): the filters every width's graph starts with run once and \
             are split, and each width's remaining filters, encoder and image sequence follow on \
             a branch of its own, so each width's frames are those its own run makes (compared \
             byte for byte on the software path and on NVENC). A shared run that fails, after \
             its standard-way retry when it was keyframe-only, falls back to one run per width, \
             and one width is always its own run. VAAPI and QSV keep one run per width, as their \
             split would share hardware frames ahead of the GPU scaler, which is not verified on \
             Intel or AMD hardware: open work (gl-1237).",
            "The argument line is logged at debug, not information, because it is logged once \
             per video and width (docs/conventions/LOGGING.md), and a run stopped for writing \
             no new frame logs no lines of its own: its reason is in the error the caller \
             logs.",
            "AMF, VideoToolbox, RKMPP and V4L2M2M have no ported filter chain, so a job \
             configured for one takes the software path. That is the owner's hardware scope \
             decision (CLAUDE.md, Current scope).",
            "A git build (ffmpeg version N-…) is read as the release its libavutil names \
             (encoder_validator.rs get_ffmpeg_version_internal) and gets -fps_mode from 5.1 \
             on, where upstream's GetFFmpegVersionInternal reads every such build as 4.4 and \
             passes -vsync, which ffmpeg 9 rejects, so no frame is made. Jellyfin bug not \
             ported (gl-1796).",
        ],
    },
    VerifiedSubsystem {
        key: "image-extraction",
        upstream_file: "MediaBrowser.MediaEncoding/Encoder/MediaEncoder.cs",
        upstream_method: "ExtractImageInternal",
        ferrofin: "The ffmpeg run that writes one image (a chapter image, a video's \
                   screen-grabbed Primary, an audio file's cover art, an embedded image \
                   stream): ferrofin-mediaencoding encoder/media_encoder.rs \
                   extract_video_image, extract_image_stream and extract_audio_image \
                   (ExtractVideoImage's two overloads and ExtractAudioImage, through \
                   ExtractImage: the I-frame attempt, then the standard one, for a video \
                   frame or an image stream, the standard one alone for audio; a video \
                   frame maps no stream, and an image stream is mapped and passed as the \
                   video stream), \
                   extract_image_arguments and hdr_tone_map (ExtractImageInternal's \
                   bwdif, 3D or plain scaler, thumbnail=n=24 and tone-map chain, the \
                   -map, -fps_mode or -vsync from video_sync_option (GetVideoSyncOption \
                   \"-1\"), -ss from format_time_parameter (GetTimeParameter), -skip_frame \
                   nokey, and the outermost -f from encoding_helper/helper.rs input_format \
                   (EncodingHelper.GetInputFormat)), image_resolution_parameter \
                   (GetImageResolutionParameter, never for audio), and run_image_extraction \
                   with image_extraction_timeout (ImageExtractionTimeoutMs, else \
                   DefaultHdrImageExtractionTimeout when the frame is tone-mapped and \
                   DefaultSdrImageExtractionTimeout otherwise), compared with ExtractImage, \
                   ExtractVideoImage, ExtractAudioImage, ExtractImageInternal, \
                   GetImageResolutionParameter, GetTimeParameter and GetInputPathArgument (disc.rs \
                   input_path_argument) and, in MediaBrowser.Controller/MediaEncoding/ \
                   EncodingHelper.cs, GetInputFormat and GetVideoSyncOption, at the pinned v12.4 \
                   tag. \
                   Which stream, offset and 3D format the callers pass (the chapter image \
                   extractor, the library scan's screen grab and its audio and embedded image \
                   providers) is not part of this row.",
        date: "2026-11-16",
        divergences: &[
            "ffmpeg runs with -v error, not upstream's -v quiet, so a failed extraction's \
             error quotes the end of ffmpeg's output, where upstream's says only that \
             extraction failed for the input; a run that succeeds is not reported either way.",
            "The -vf graph is passed quoted, where upstream passes it bare, so the 3D scalers' \
             spaces split it into stray ffmpeg arguments and a 3D video gets no image. A \
             Jellyfin bug, deliberately not ported.",
            "On ffmpeg 5.1 or later the kept frame is converted to the BT.601 matrix a JPEG \
             decoder reads (scale=out_color_matrix=bt601, last in the graph, for audio cover \
             art and image streams too), where upstream converts none, so its BT.709 and \
             tone-mapped frames shift colour (gl-1158): a Jellyfin bug, deliberately not \
             ported.",
            "An offset of a day or more seeks to its full hour count, where GetTimeParameter's \
             hh format drops the days and seeks into the first 24 hours. A Jellyfin bug, \
             deliberately not ported.",
            "One thumbnail-pool slot covers both attempts of a video frame or an image stream, \
             where upstream takes the pool again for the standard attempt, so a fallback does \
             not queue behind other extractions.",
            "A failed I-frame attempt is logged at warning only when ffmpeg timed out or could \
             not start; one that wrote no image, the routine case, falls back quietly, where \
             upstream logs every failed attempt at warning: a warning per chapter would scale \
             with library size (docs/conventions/LOGGING.md).",
            "The temp directory is checked to be writable before ffmpeg runs, and the error \
             names it, where upstream creates it and lets ffmpeg fail to write the image; a \
             failed attempt's partial or empty file is deleted, where upstream leaves it there.",
            "A git build (ffmpeg version N-…) is read as the release its libavutil names \
             (encoder_validator.rs get_ffmpeg_version_internal) and gets -fps_mode from 5.1 \
             on, where upstream's GetFFmpegVersionInternal reads every such build as 4.4 and \
             passes -vsync, which ffmpeg 9 rejects, so no image is made. Jellyfin bug not \
             ported (gl-1796).",
        ],
    },
    VerifiedSubsystem {
        key: "websocket-streams",
        upstream_file: "MediaBrowser.Controller/Net/BasePeriodicWebSocketListener.cs",
        upstream_method: "HandleMessages",
        ferrofin: "handlers/websocket.rs websocket_upgrade runs handle_socket under the upgrade \
                   request's culture and UI culture (WebSocketManager's RequestUICulture, \
                   applied by ApplyRequestCulture before each inbound message and, in \
                   SendDataForConnectionAsync, before each GetDataToSendForConnection), and \
                   task_transition_message re-shows a captured snapshot \
                   in it through TaskManager::localize_tasks (ferrofin-core scheduled_tasks.rs \
                   show_text, sort_by_name); handle_socket → Streams::apply, event_stream, wake_due \
                   (drain, should_send); sessions_wake (session_event_forces), sessions_message → \
                   ferrofin-core session_manager.rs SessionEvent raises, with upsert_session's \
                   SessionActivity past SESSION_ACTIVITY_INTERVAL (LogSessionActivity, a new \
                   session included) and report_capabilities' CapabilitiesChanged \
                   (ReportCapabilities); activity_wake → ferrofin-core activity_manager.rs \
                   create_entry's entry-created raise (ActivityManager.CreateAsync EntryCreated); \
                   update_task_subscription, subscribe_tasks, task_transition_message, \
                   task_progress_wake → ferrofin-core scheduled_tasks.rs subscribe_task_updates, \
                   publish_snapshot from claim (TaskExecuting), record_result and stop_run \
                   (TaskCompleted), and \
                   TaskProgress::report's progress raise \
                   (ScheduledTaskWorker.OnProgressChanged TaskProgress). Compared with the \
                   Sessions listener \
                   (Jellyfin.Api/WebSocketListeners/SessionInfoWebSocketListener.cs), the \
                   ActivityLog listener \
                   (Jellyfin.Api/WebSocketListeners/ActivityLogWebSocketListener.cs) and the \
                   ScheduledTasks listener \
                   (Jellyfin.Api/WebSocketListeners/ScheduledTasksWebSocketListener.cs); \
                   envelope serializes each frame's DTOs directly, in declaration order, as \
                   WebSocketConnection.SendAsync's SerializeToUtf8Bytes does (property order \
                   compared with MediaBrowser.Model/Dto/SessionInfoDto.cs, \
                   Session/PlayerStateInfo.cs and Tasks/TaskInfo.cs)",
        date: "2026-11-05",
        divergences: &[
            "Events already queued for a socket are drained and answered with one Sessions or \
             ActivityLogEntry frame, where upstream sends one per forced event: a burst yields \
             fewer frames with the same final state.",
            "A non-administrator's Sessions frames carry the hardware accelerator blanked, as \
             GET /Sessions does; upstream's stream sends it as is.",
            "A second SessionsStart, ActivityLogEntryStart or ScheduledTasksInfoStart on a \
             socket replaces its subscription; upstream adds a second one and sends each frame \
             twice.",
            "ScheduledTasksInfoStart sends the current task list at once, where upstream's Start \
             sends nothing until a task event, because jellyfin-web's scan button has no HTTP \
             fallback; it costs one registry snapshot per subscribe and none per period, and counts \
             as the stream's last send, so progress inside the first period is held back.",
            "A task start or completion is sent with the task list captured at the transition \
             (its names, descriptions and categories shown again in the socket's culture), \
             where upstream reads the list when it sends, so a start frame never carries the \
             run's first progress report, which upstream's can; a progress frame due while one \
             is still queued is dropped, so a short run never shows out of order, and a later \
             report sends once the period has passed.",
            "A socket's task list is ordered in its own upgrade request's culture. Upstream's one \
             send loop per listener orders every socket's list in the CurrentCulture it inherited \
             when the listener was built, on the first request after boot to reach the WebSocket \
             middleware (any request, not only a /socket upgrade); no client shows the order as \
             sent, since jellyfin-web sorts the tasks itself (owner decision 2026-11-05, gl-428).",
            "A session list that cannot be built is logged and skipped and the subscription stays; \
             upstream logs it and drops the connection's subscription.",
            "A session's NowPlayingItem map members (ImageTags, ProviderIds, ImageBlurHashes, \
             RequiredHttpHeaders, and Trickplay's media source ids) are written in hash order, \
             as on every item endpoint, not upstream's insertion order; Trickplay lists each \
             source's widths largest first, as the trickplay row says.",
            "A *Start whose Data is missing, has no second field or holds a field that does not \
             parse as a whole number still subscribes, at jellyfin-web's 1500 ms when the period \
             is at fault, where upstream's Start throws and makes no subscription.",
            NOW_PLAYING_ITEM_BUILT_ON_READ,
        ],
    },
    VerifiedSubsystem {
        key: "images",
        upstream_file: "src/Jellyfin.Drawing/ImageProcessor.cs",
        upstream_method: "GetImageCacheTag",
        ferrofin: "The image cache tags (ImageTags, BackdropImageTags, the Parent*, Series*, \
                   Album* and People[] tags, ImageBlurHashes' keys, Chapters[].ImageTag, \
                   search hints, GET /Items/{itemId}/Images, ChannelPrimaryImageTag, a user's \
                   PrimaryImageTag); image serving and processing are compared per \
                   operation, apart from the Live TV artwork rule below (ferrofin-livetv \
                   projection.rs is_artwork_url, channel_images_update, \
                   program_images_update, manager.rs add_provider_metadata; ferrofin-traits \
                   item_image_info.rs live_tv_image_is_internal, read by handlers/images.rs \
                   live_tv_image_is_servable, ferrofin-providers provider_manager.rs \
                   delete_image and ferrofin-core dto_service.rs primary_image_aspect_ratio, \
                   dynamic_images.rs local_images and item_repository.rs \
                   swap_item_images). ferrofin-drawing processor.rs \
                   get_image_cache_tag (the owning \
                   item's path, null as empty) and get_image_cache_tag_for_path (md5 of the \
                   UTF-16 path + DateModified ticks, as a .NET Guid \"N\") ← ferrofin-core \
                   dto_service.rs image_tag, tag_and_fill_blur_hash (attach_images, \
                   add_inherited_images, parent_primary_tag, attach_people with \
                   person_paths) over the page's one image read, load_images_batch, which \
                   lists each stored image once (ferrofin-db padded_in_chunks binds each id \
                   once), the chapter tags (the stored item path, \
                   virtual_paths reverse); search_manager.rs fill_artwork; \
                   handlers/images.rs image_info_dto; ferrofin-livetv manager.rs \
                   channel_image_tags (a channel has no path); user_manager.rs get_user_dto \
                   (the profile image's own path). Item paths as item_repository.rs mapped \
                   reads them (BaseItemMapper.Map's ExpandVirtualPath), and every stored \
                   image row through item_repository.rs image_info_from_row (the DTO page \
                   read, GET /Items/{itemId}/Images and the scan's change detection; \
                   BaseItemMapper.MapImageFromEntity: the path expanded, the type cast as \
                   stored). Compared with \
                   Jellyfin.Server.Implementations/Item/BaseItemMapper.cs \
                   (MapImageFromEntity), \
                   Emby.Server.Implementations/Dto/DtoService.cs (GetImageCacheTag, \
                   GetPrimaryImageAspectRatio, \
                   GetTagAndFillBlurhash, GetTagsAndFillBlurhashes, AddInheritedImages, \
                   AttachPeople), \
                   Jellyfin.Server.Implementations/Item/ChapterRepository.cs (Map), \
                   Jellyfin.Api/Controllers/ImageController.cs (GetItemImageInfos), \
                   Jellyfin.Api/Controllers/SearchController.cs (GetSearchHintResult), \
                   MediaBrowser.Controller/Entities/BaseItem.cs (DeleteImageAsync, \
                   SwapImagesAsync), \
                   Emby.Server.Implementations/Images/BaseDynamicImageProvider.cs \
                   (GetStripCollageImagePaths), \
                   src/Jellyfin.LiveTv/LiveTvDtoService.cs (GetImageTag), \
                   src/Jellyfin.LiveTv/Guide/GuideManager.cs (GetChannel, UpdateImage), \
                   MediaBrowser.Controller/Entities/BaseItemExtensions.cs (SetImagePath) and \
                   Jellyfin.Server.Implementations/Users/UserManager.cs (GetUserDto). Also the \
                   download of a remote image: ferrofin-providers image_download.rs \
                   open_image (guarded_http.rs GuardedHttp::send, which follows and vets each \
                   redirect; destination.rs ImageDestinations), provider_manager.rs \
                   cached_remote_image, and the scan's downloads of a credited person's \
                   profile photo (the credit's URL from TheMovieDb, TheTVDB, or an NFO \
                   actor's <thumb> through library_scan.rs person_to_entity) and a studio's \
                   thumb (ferrofin-core library_scan.rs \
                   download_images and studio_thumb, named from the resolved type through \
                   tmdb.rs TmdbClient::download and studios.rs StudiosClient::download; \
                   enrich_people and enrich_studio_images hand the file to ferrofin-providers \
                   image_save.rs ImageFileSaver through finish_artwork, which puts a person's \
                   or a studio's new art beside its own folder while that is in the server's \
                   own directories, and otherwise follows the library's opt-in; rows stored \
                   before keep their art-folder files), and the scan's save of the artwork an \
                   NFO names by URL, an item's <thumb> and <fanart><thumb> \
                   (library_scan.rs fetch_local_nfo's LocalNfo.remote_images, saved by \
                   save_nfo_images in collect_artwork before the image providers run, for each \
                   type the item has no image of, whatever the library's image fetchers and \
                   per-type limits, as upstream; downloaded through tmdb.rs \
                   TmdbClient::download, written by artwork.rs write_acquired_image and \
                   published by finish_artwork's image saver), \
                   compared with \
                   MediaBrowser.Providers/Manager/ProviderManager.cs (SaveImage), \
                   MediaBrowser.Providers/Manager/ItemImageProvider.cs (EnableImageStub, \
                   SaveImageStub: a Person and a Studio are IItemByName, so their remote \
                   images reach SaveImage through a stub; MergeImages, UpdateReplaceImages and \
                   RefreshImages), \
                   MediaBrowser.Providers/Manager/ImageSaver.cs (SaveImage, \
                   GetStandardSavePath), \
                   Jellyfin.Server/Startup.cs (the NamedClient.Default handler), \
                   MediaBrowser.Controller/Extensions/XmlReaderExtensions.cs \
                   (GetPersonFromXmlNode), \
                   MediaBrowser.XbmcMetadata/Parsers/BaseNfoParser.cs (FetchThumbNode), \
                   MediaBrowser.Providers/Manager/MetadataService.cs (RefreshWithProviders' \
                   local-provider loop, MergePeople) and \
                   Emby.Server.Implementations/Library/LibraryManager.cs \
                   (SavePeopleMetadataAsync, UpdateImagesAsync, ConvertImageToLocal)",
        date: "2026-11-14",
        divergences: &[
            "A stored blurhash that is not UTF-8 reads as none, and the scan computes it \
             again, where MapImageFromEntity's Encoding.UTF8.GetString replaces the \
             invalid bytes with U+FFFD and sends the garbled hash. An empty one reads as \
             none too: \
             GetTagAndFillBlurhash drops it as well, so only GET /Items/{itemId}/Images \
             differs, its BlurHash absent instead of \"\". Jellyfin bug not ported.",
            "Decoding and resizing run under limits upstream does not have (owner decision \
             2026-10-31, gl-1097), the settings image_decode_max_megapixels, \
             image_decode_max_memory_mb and image_resize_budget_mb, because a failed allocation, \
             which Skia reports as an exception, ends a Rust process. An image past one is \
             served as its original file, as ImageProcessor.ProcessImage's catch-all does with \
             any failed encode, and one too large to decode has no blur hash and is left out of \
             a collage or the splashscreen.",
            "A remote image is downloaded under a limit upstream does not have (owner decision \
             2026-10-31, gl-1095), the setting image_download_max_mb: ProviderManager.SaveImage \
             reads any body into memory. Past the limit it is refused and the read stops. \
             cached_remote_image streams to a file, so it holds no body in memory. As upstream, \
             one request at a time downloads a URL (_imageSaveLock, around cached_remote_image \
             and save_image_from_url), but its 10 s memory cache of the bytes is not ported: an \
             image request has the file instead, and a second save of a URL downloads it again.",
            "A remote image's first bytes must be an image whatever the response declares \
             (owner decision 2026-10-31, gl-1095), where SaveImage trusts any image/* type: the \
             first file stored for a URL or an item stays there. The formats are listed in \
             image_download.rs is_image, with SVG and tvheadend's untyped /imagecache/ URLs \
             allowed.",
            "A person's profile photo or a studio's thumb that the scan downloads is named \
             from its type only when that is JPEG, PNG, WebP or GIF, the formats the scan's \
             art-folder readers know (library_scan.rs ART_FILE_EXTENSIONS), and any other \
             image is stored as .jpg. ImageSaver.GetStandardSavePath names every type from \
             MimeTypes.ToExtension and throws for one it has no extension for.",
            "A credited person's profile-photo URL that gave the scan no photo, whatever the \
             failure, is not asked again by a later credit until a restart (gl-1477, decided at \
             triage; it counts a 404, a 403 and a non-image as answers, as gl-1095 does for a \
             remote image): the person's own refresh and the people task's missing-image \
             refresh still ask. Upstream's SavePeopleMetadataAsync stores the URL as a remote \
             Primary, which ConvertImageToLocal removes after a 404, a 403 or a non-image, so \
             the next crediting save asks again, and keeps after any other HTTP or I/O \
             failure, so no later credit asks.",
            "An NFO actor's <thumb> that names a local file (the path an NFO saver writes, \
             Kodi's .actors folder) is not the credit's photo: person_to_entity keeps only an \
             http(s) URL, so a remote provider's photo URL for the same credit fills it \
             (merge_people) and is downloaded, and with none the person gets no photo. \
             Upstream's MergePeople keeps the path over the remote URL and \
             SavePeopleMetadataAsync stores it as the Primary's path. Fix path: gl-1818, \
             which first needs the owner's decision on which files an NFO may name.",
            "An item's NFO <thumb> that names a local file is not read, where FetchThumbNode \
             resolves any existing file (IDirectoryService.GetFile) and RefreshWithProviders \
             sets it with MergeImages; an album's or an artist's NFO artwork URLs are not \
             saved, where the same loop saves every kind's. Fix path for both: gl-1818; the \
             local file first needs the owner's decision on which files an NFO may name.",
            "Under \"Replace existing images\", an NFO's fanart saved as a Backdrop is kept and \
             the item's earlier downloaded backdrops are replaced, as for any replaced type, \
             while the image providers only fill the backdrops the limit has left. Upstream \
             takes Backdrop out of the replace set once the NFO saved one, so it keeps the \
             earlier backdrops and adds another copy of the NFO's to them on every such \
             refresh. Jellyfin bug not ported.",
            "A remote image download refuses loopback, private, link-local, CGNAT and reserved \
             addresses, on its first request and on every redirect it follows (owner decision \
             2026-10-31, gl-1096), unless image_private_http_allow lists the host, its address \
             or its network, or the host is a configured Live TV tuner's; upstream's \
             NamedClient.Default (Startup.cs) has no destination policy and follows redirects \
             anywhere. A refusal counts as an answer, as a 404 does. A download follows at \
             most 10 redirects, reqwest's default, where .NET's handler follows 50. After a \
             redirect, SaveImage's fallback to the URL's suffix is not taken: a response with no \
             type, or application/octet-stream, is refused, but for the tvheadend \
             /imagecache/ workaround, which still names PNG.",
            "A row whose URLs all gave no image stays, and each URL is left alone for \
             image_negative_cache_ttl_secs (owner decision 2026-10-31, gl-1095), where \
             LibraryManager.ConvertImageToLocal's removeOnFailure removes the row, for the same \
             reason, so that a refresh must bring it back. The same answers count: a 404, a 403 \
             and a non-image, plus a body past the limit or failing the content check. Any \
             other failure throws there and moves on to the next URL here, and neither removes \
             the row or remembers the URL.",
            "A timer with a blank name (the new-timer defaults without a programId, or a \
             series timer, a series timer's timer or a programme whose name is blank) runs no \
             lookup by name: ferrofin-livetv manager.rs fill_images finds artwork only by a \
             listings series id then, where LiveTvDtoService.FillImages filters on nothing and \
             takes the library's first series' Thumb and Backdrop, and the first series' Primary \
             (a series timer's) or the guide's first programme's (a programme's). Fix path: \
             gl-1790, an owner decision.",
            "A tuner's or guide's artwork is kept only as an absolute http or https URL, and a \
             channel's or programme's stored row that is neither a URL nor a file in the \
             internal metadata directory is neither served nor listed. Upstream's SetImagePath \
             also accepts a local file path there. Jellyfin bug not ported.",
            "The other uses of such a stored row stop at the same directory. DELETE removes the \
             row and leaves the file, where DeleteImageAsync deletes any local file. A Primary \
             with no stored size reports the kind's default PrimaryImageAspectRatio, as a file \
             that cannot be read does, where GetPrimaryImageAspectRatio opens it. A collage \
             skips it, where GetStripCollageImagePaths draws any local file. A reorder \
             exchanges two rows' files only when both are in that directory, and otherwise does \
             nothing, where SwapImagesAsync swaps any local files. Same Jellyfin bug as the \
             entry above, not ported.",
        ],
    },
    VerifiedSubsystem {
        key: "user-data",
        upstream_file: "Emby.Server.Implementations/Library/UserDataManager.cs",
        upstream_method: "SaveUserData",
        ferrofin: "The write path, plus the read path only where a page's version groups get \
                   their user data and where a page lists an item twice (get_user_data_dtos_for_rows \
                   resolves each item once, so every occurrence gets its stored row, as \
                   GetUserDataBatch's TryGetValue on the grouped rows does). ferrofin-core \
                   user_data_manager.rs mutate_row \
                   (SaveUserData with its retired-key update, GetUserData, ResolveUserDataRow), \
                   with the item's keys from user_data_key_repository.rs load_identities and \
                   KEY_ROWS_SQL (GetUserDataKeys, from the series a Season or an Episode \
                   names in SeriesId, else its nearest Series parent, read in the same \
                   statement as the item), save_user_data_with_reason (the \
                   UpdateUserItemDataDto SaveUserData), \
                   save_play_state and apply_play_report (UpdatePlayState), \
                   reset_playback_stream_selections (ResetPlaybackStreamSelections), \
                   record_playback_start, set_likes, mark_item_played, mark_item_unplayed, \
                   mark_container with mutate_rows (a container's members, a chunk per write \
                   transaction), clear_own_play_state and propagate_played_state. Compared with \
                   Emby.Server.Implementations/Session/SessionManager.cs (OnPlaybackStart, \
                   OnPlaybackProgress, UpdatePlaybackSettings, OnPlaybackStopped), \
                   MediaBrowser.Controller/Entities/BaseItem.cs (MarkPlayed, MarkUnplayed), \
                   MediaBrowser.Controller/Entities/Folder.cs and \
                   MediaBrowser.Controller/Entities/Movies/BoxSet.cs (MarkPlayed, MarkUnplayed), \
                   MediaBrowser.Controller/Entities/TV/Episode.cs and \
                   MediaBrowser.Controller/Entities/TV/Season.cs (Series, FindSeriesId, \
                   GetUserDataKeys), \
                   MediaBrowser.Controller/Entities/Video.cs (PropagatePlayedState) and \
                   Jellyfin.Api/Controllers/UserLibraryController.cs (MarkFavorite, \
                   UpdateUserItemRatingInternal). The version groups: dto_service.rs prefetch's \
                   version user data (a page item's from the page's own read, a version off \
                   the page from user_data_manager.rs get_stored_user_data_batch), compared \
                   with Emby.Server.Implementations/Library/UserDataManager.cs \
                   (GetUserDataBatch, whose cache answers the page's items, and \
                   GetResumeUserDataBatch), Emby.Server.Implementations/Dto/DtoService.cs \
                   (GetBaseItemDtos) and Emby.Server.Implementations/Library/\
                   MediaSourceManager.cs (SetAlternateVersionResumeStates). After a scan or \
                   boot-repair fold, item_persistence_service.rs agree_user_data (the \
                   retired-key update of SaveUserData, from the row ResolveUserDataRow reads), \
                   compared with Jellyfin.Server.Implementations/Item/\
                   ItemPersistenceService.cs (ReattachUserDataAsync, ReconcileUserDataAsync). \
                   Also the read path of the folder played test: item_repository.rs \
                   get_is_played with translate_query.rs build_is_played_query \
                   (resolve_descendant_query's \
                   closure and links, each item through push_leaf_is_played_per_row), compared \
                   with Jellyfin.Server.Implementations/Item/BaseItemRepository.Querying.cs \
                   (GetIsPlayed), BaseItemRepository.TranslateQuery.cs \
                   (BuildLeafIsPlayedFilter), src/Jellyfin.Database/\
                   Jellyfin.Database.Implementations/DescendantQueryHelper.cs \
                   (GetAllDescendantIds) and MediaBrowser.Controller/Entities/Folder.cs \
                   (IsPlayed)",
        date: "2026-11-11",
        divergences: &[
            "After a scan or boot-repair fold, which upstream does not have (it deletes the \
             duplicate, and ReconcileUserDataAsync gives the user's rows the most recent play when \
             its detached history reattaches), each user's rows of the kept item take the values \
             of the row the DTO reads (the first current key the user holds, else the most recent \
             play), so an explicit unplay under that key stands against an older row's later \
             play.",
            "Each change reads the row inside its write transaction, so two overlapping saves of \
             one item both land; upstream's GetUserData hands each caller a copy and SaveUserData \
             writes every column back, so there the later save undoes the earlier one.",
            "Marking a container unplayed also clears the play state on the container's own rows, \
             which earlier Ferrofin builds wrote; upstream's MarkUnplayed leaves them, so a Played \
             set on a container through the UserData API survives it there. Fix path: gl-1064.",
            "A container's members are saved a chunk at a time, each chunk in one write \
             transaction, where upstream's Folder.MarkPlayed commits each member's SaveUserData \
             alone and stops at the first that fails: here a member's write that fails also rolls \
             back the members of its chunk saved before it, and a version's save that fails \
             leaves the chunk's later members saved (gl-1321).",
        ],
    },
    VerifiedSubsystem {
        key: "access-filtering",
        upstream_file: "MediaBrowser.Controller/Entities/BaseItem.cs",
        upstream_method: "IsVisibleStandaloneInternal",
        ferrofin: "The library half of access filtering: which libraries a user may open, and \
                   what a request naming an item may reach; the rating and tag rules are \
                   compared per operation. ferrofin-core item_visibility.rs visible, \
                   standalone, folder_allowed (BaseItem.IsVisibleStandalone, Folder.IsVisible, \
                   CollectionFolder.IsVisible, BoxSet.IsVisibleStandalone, \
                   Playlist.IsVisibleStandalone) and user_view_manager.rs \
                   get_user_root_children (Folder.GetChildren, \
                   UserRootFolder.GetEligibleChildrenForRecursiveChildren); item_repository.rs \
                   visible_view_rows → library_enabled under scope_to_user_libraries, \
                   root_folder_top_parents and grouped_libraries (GetChildren(user, true) in \
                   AddUserToQuery's GetUserViews, UserViewManager.HasVisibleItem and \
                   GetTopParentIdsForQuery's grouping arm), and UserViewMaterialisation, the \
                   scope's get_user_views_with_query call that writes the user's views as \
                   AddUserToQuery's GetUserViews call writes them (LibraryManager.AddUserToQuery, \
                   UserViewManager.GetUserViews, compared 2026-11-10), with \
                   user_view_manager.rs ensure_user_playlists_view and view_refresh_due dating \
                   and refreshing the Playlists view (LibraryManager.GetNamedView, compared \
                   2026-11-11), reading a library's options at its \
                   Path expanded as BaseItemMapper.Map expands it \
                   (CollectionFolder.GetLibraryOptions, compared 2026-11-05); handlers/items.rs \
                   require_scope_visible, require_scope_id_visible, retain_visible_ids at the \
                   routes that take a scope (ItemsController.GetItems, \
                   LibraryManager.AddUserToQuery); handlers/streaming.rs \
                   require_streamable_item at the stream, HLS, universal audio, subtitle and \
                   attachment routes (StreamingHelpers.GetStreamingState, \
                   SubtitleController.GetSubtitle, VideoAttachmentsController.GetAttachment); \
                   handlers/media_info.rs playback_info (MediaInfoController.GetPlaybackInfo, \
                   GetPostedPlaybackInfo); item_repository.rs \
                   resolve_views_with_parent and item_visibility.rs hidden_playlists on \
                   listings that can hold a playlist (Folder.QueryRecursive, \
                   Folder.QueryWithPostFiltering, Playlist.IsVisible); dto_service.rs \
                   get_base_item_dtos' IsVisible pass (DtoService.GetBaseItemDtos), which of \
                   the handlers GET /Items/Suggestions and the six /{kind}/{itemId}/Similar \
                   routes ask for, as their controllers do, and GET /Movies/Recommendations \
                   runs through filter_visible_items before its one projection; \
                   handlers/live_tv.rs query_programs_inner \
                   (LiveTvController.GetLiveTvPrograms, GetPrograms)",
        date: "2026-11-01",
        divergences: &[
            "A listing with a user leaves out the playlists that fail \
             Playlist.IsVisibleStandalone, by id in its SQL, so its paging and \
             TotalRecordCount stay right: GET /Items, GET /Search/Hints and the facet routes \
             over the same query. Upstream checks only a recursive query limited to BoxSet \
             and Playlist, so a mixed one lists every user's private playlists. GET \
             /Items/Suggestions hides them upstream too, through IsVisible on the page; here \
             they leave the SQL as well, so these rows no longer shorten a non-administrator's \
             page or count in its total. An administrator keeps upstream's answer on every \
             one of these routes. Jellyfin bug not ported (owner decision 2026-10-31, gl-831). TODO(gl-1127): box sets \
             hidden by BoxSet.IsVisible are still listed, as upstream.",
            "A request naming an item as its scope (parentId, ids, seriesId, appearsInItemId) \
             stops at the libraries the user may open, on every route that takes one: a \
             library or view must pass IsVisible and anything else IsVisibleStandalone, or \
             the request answers 403, a BoxSet or Playlist browse included, and ids are \
             reduced to those that pass IsVisibleStandalone. Upstream applies IsVisible \
             alone, on GET /Items and GET /Trailers, and answers 401. Jellyfin bug not ported \
             (owner decisions 2026-10-31 and 2026-11-01, gl-1091, gl-1136).",
            "A request naming a user is held to that user on the stream, HLS, universal audio, \
             subtitle Stream.{format} and attachment routes: the default policy runs (a disabled \
             account 401, remote access and the access schedule 403), and the item, and the \
             version a mediaSourceId names, must pass IsVisibleStandalone or the route answers \
             404. Upstream's GetStreamingState reads the item with no user, and its stream and \
             Stream.{format} actions are anonymous; only its attachment action already checks \
             the item for a user, and neither the policy nor the version. A channel takes the \
             same check, as PlaybackInfo does. A DVR recording has no row here, so it takes \
             Live TV access, IsVisibleStandalone over its entity, the libraries holding its \
             file (found as GetCollectionFolders finds them, ignoring case) and then its \
             channel. GET and POST PlaybackInfo use that rule for DVR ids as well, resolving \
             sources through the recording media-source manager. GET and DELETE \
             /LiveTv/Recordings/{recordingId} share the visibility check, and GET \
             /LiveTv/Recordings lists by it. Upstream addresses recordings by their \
             library item ids and has no separate DVR id to resolve. On the routes upstream \
             leaves anonymous, no credential, a token that names nobody and an API key \
             stay so, which binds a client that sends its token. The legacy /hls/ \
             routes are outside this: they serve a transcode's file by its random name, and \
             the item id selects nothing, as upstream. Deliberate divergence (owner decision \
             2026-10-31, gl-1090).",
            "An unscoped query for a user writes the user's views (the Playlists view, a \
             grouped view, the Folders view, the Live TV view), where upstream's \
             AddUserToQuery calls GetUserViews on every such query: for the life of the \
             process, once per user and per set of root folders the user may see. Those \
             writes create a missing row or rename a Playlists, grouped, Folders or Live TV \
             view whose name changed, so the rows end the same, except that a change while the set \
             stays the same (for example a view row deleted, the Folders view enabled, a new \
             server culture, a changed grouped-folders preference, or Live TV becoming \
             available to the user), or a write that failed, is written by the next GET \
             /UserViews rather than the next query; a write its query was cancelled in is \
             retried. The query \
             that writes them is scoped by the rows read before the write (gl-1584). \
             GetNamedView's refresh of the Playlists view (when it is new, 24 hours after its last refresh, \
             or once its display parent was saved since) is the view's save, made at once \
             rather than queued (gl-1625); the collage that refresh draws upstream is \
             TODO(gl-1656), and the other views' dates and refresh TODO(gl-1655). It falls \
             due only where the views are read: on every GET /UserViews, and on the query \
             path when that writes them, so a refresh due while the set stays the same \
             waits for the next GET /UserViews.",
            "GET and POST /LiveTv/Programs read librarySeriesId's row as the request's user, or \
             as the signed-in caller when a POST body names no user, so a series that user \
             cannot open (library, rating or tag rules) answers like an id that is no series: \
             IsSeries stays forced and its title is not searched for. Upstream's \
             GetItemById<Series> reads it with no user. A POST body naming no user keeps the \
             query without one, as upstream, so its DTOs carry no user data and no user's \
             parental restrictions filter its airings (gl-1475 asks whether to diverge); only \
             a request with no user at all (an API key naming none) still reads the row as \
             upstream does. Jellyfin bug not ported (owner decision 2026-10-31, gl-1091; gl-1113, \
             gl-1130).",
        ],
    },
    VerifiedSubsystem {
        key: "progressive-transcode",
        upstream_file: "Jellyfin.Api/Controllers/VideosController.cs",
        upstream_method: "GetVideoStream",
        ferrofin: "Which GET and HEAD requests on the /Videos and /Audio stream routes reach \
                   ffmpeg, who may send them, and when the progressive body they answer with \
                   ends; the query parameters of those operations are not part of this row. \
                   handlers/streaming.rs stream_route_container (the stream.{container} \
                   literal, in any case, as ASP.NET matches it), static_or_transcode and \
                   serve_static_source (the static branches of VideosController.GetVideoStream \
                   and AudioHelper.GetAudioStream: open live stream, remote source, other \
                   protocol, file, and for video alone the disc-folder exception), with \
                   static_content_type, the type a file answers with (GetMimeType(\".\" + \
                   OutputContainer, false) ?? GetMimeType(MediaPath), the container as \
                   StreamingHelpers.GetStreamingState sets it, compared 2026-11-13; with \
                   the MimeTypes package's whole table and GetMimeType(MediaPath)'s \
                   application/octet-stream when neither names a type, compared \
                   2026-11-14) and \
                   handlers/hls.rs transcode_stream_fallback \
                   (FileStreamResponseHelpers.GetTranscodedFile, which answers a HEAD before \
                   ffmpeg), which the transcode branch of /Audio/{itemId}/universal also \
                   reaches (UniversalAudioController, [Authorize]), with the body it answers \
                   with, streaming.rs serve_growing_transcode \
                   (MediaBrowser.Controller/Streaming/ProgressiveFileStream.cs StopReading, \
                   compared 2026-11-05); the default policy is auth.rs RequireAuth \
                   (DefaultAuthorizationHandler). Also the admission of a \
                   job on those routes and the HLS ones: ferrofin-mediaencoding \
                   transcoding/manager.rs start_ffmpeg, compared with \
                   MediaBrowser.MediaEncoding/Transcoding/TranscodeManager.cs (StartFfMpeg, \
                   PingTimer, KillTranscodingJobs; a start whose ffmpeg exits non-zero, with \
                   OnTranscodeBeginRequest, compared 2026-11-11; the ffmpeg log's name, \
                   ffmpeg_log_file_name, with the request's MediaSourceId as sent, compared \
                   2026-11-14) and \
                   Emby.Server.Implementations/Session/SessionManager.cs (MaxActiveSessions)",
        date: "2026-11-02",
        divergences: &[
            "A GET that would start a transcode on /Videos/{itemId}/stream, \
             /Videos/{itemId}/stream.{container}, /Audio/{itemId}/stream or \
             /Audio/{itemId}/stream.{container} needs a caller, and so does a static video \
             request for a Blu-ray or DVD folder, which only ffmpeg reads: with no user and no \
             API key it answers 401, and the default policy runs (a disabled account 401, \
             remote access and the access schedule 403). Upstream's actions are anonymous \
             and start ffmpeg for such a request. A HEAD and a static read stay anonymous as \
             upstream, and a TranscodingUrl carries ApiKey. The lowercase api_key counts only \
             with EnableLegacyAuthorization on, so it is no caller by default. Deliberate \
             divergence (owner decision 2026-10-31, gl-1089).",
            "Jobs re-encoding any stream run under caps upstream does not have (owner decisions \
             2026-10-31 and 2026-11-01, gl-1098), the settings max_transcodes and \
             max_transcodes_per_user, both 0, off, by default, which leaves StartFfMpeg's \
             behaviour as upstream's: it starts whatever it is asked to, and MaxActiveSessions \
             applies only at login. Past a cap a start ends the longest-idle job (no active \
             request) of the same user on the same device, the teardown the ping timeout \
             would have brought, and with none idle answers 503 with Retry-After: 10 before \
             anything is created. The device is the request's DeviceId, compared ignoring \
             ASCII case (KillTranscodingJobs uses OrdinalIgnoreCase), and a start without one \
             ends nothing: an HLS player between two segment requests is idle too, so \
             another device's job is never ended (gl-1140). A job copying every stream is \
             free. The user is the authorization's, as upstream's StartFfMpeg userId is; an \
             API key has none, so it is outside the per-user cap and inside the server-wide \
             one.",
            "A job whose ffmpeg exits non-zero during its start stays registered, as upstream, \
             so a retry serves its output and ends at once and a stop report deletes it, but \
             the idle reaper also takes it with its output once nothing has read or pinged it \
             for its ping timeout: upstream's StartFfMpeg throws before a stream holds the job, \
             so its ActiveRequestCount stays 1 (a retried HLS segment's \
             WaitForActiveTranscodingRequests then waits until the request is cancelled) and \
             only KillTranscodingJobs removes it. A later start of the same output replaces \
             the exited job, where upstream adds the new one beside it and \
             OnTranscodeBeginRequest still finds the exited one first. Jellyfin bugs not \
             ported (gl-1373).",
        ],
    },
    VerifiedSubsystem {
        key: "stream-parameter-patterns",
        upstream_file: "Jellyfin.Api/Controllers/DynamicHlsController.cs",
        upstream_method: "GetMasterHlsVideoPlaylist",
        ferrofin: "Which values of the HLS and stream routes' [RegularExpression] parameters are \
                   refused, and the 400 they answer with. extract.rs StreamPattern \
                   (EncodingHelper.ContainerValidationRegexStr and LevelValidationRegexStr, matched \
                   over the whole value as RegularExpressionAttribute matches them, with an empty \
                   value passing), pattern_complaint and validation_failure (the [ApiController] \
                   model-state ValidationProblemDetails: one body naming every failing parameter by \
                   its declared name, whatever case the query spelled it in), each answered after \
                   authorization (RequireAuth on the HLS and universal routes; the stream routes \
                   are anonymous upstream) and before the item is looked up. handlers/hls.rs \
                   stream_pattern_failures, route_container_failures and segment_pattern_failures \
                   in the seven DynamicHlsController actions (GetMasterHlsVideoPlaylist, \
                   GetVariantHlsVideoPlaylist, GetLiveHlsStream, GetMasterHlsAudioPlaylist, \
                   GetVariantHlsAudioPlaylist, GetHlsVideoSegment, GetHlsAudioSegment): \
                   segmentContainer, audioCodec, level, videoCodec and subtitleCodec on each, and \
                   container on GetLiveHlsStream's query and on a segment's route {container} (the \
                   text after the last dot of {segmentId}.{container}); the playlists declare no \
                   container, so their container query is not read. A route container is \
                   [Required] too: extract.rs route_container_complaint answers a blank one with \
                   [Required]'s complaint alone (the pattern passes the null the binder makes of \
                   it), filed beside the others. The same stream_pattern_failures in \
                   handlers/videos.rs and audio.rs get_video_stream and get_audio_stream, and \
                   route_container_failures in get_video_stream_by_container and \
                   get_audio_stream_by_container (VideosController.GetVideoStream and \
                   GetVideoStreamByContainer, AudioController.GetAudioStream and \
                   GetAudioStreamByContainer), with container the query's or the \
                   stream.{container} route's, for anonymous and Static=true requests too, and \
                   again in transcode_stream_fallback; live_tv.rs get_live_stream_file \
                   (LiveTvController.GetLiveStreamFile, anonymous: its route container, beside a \
                   blank streamId in one body, and a segment without the stream. literal no match, \
                   a 404); and audio.rs \
                   get_universal_audio_stream (UniversalAudioController.GetUniversalAudioStream: \
                   audioCodec and transcodingContainer only). Compared with those actions' parameter \
                   attributes at the pinned v12.4 tag, and settled against a stock 12.4 container \
                   (2026-11-14): bodies and keys for each parameter, an encoded line break, a leading \
                   space, 41 and 40 characters, a blank value, a later repeat of the key, the \
                   undeclared container query of the playlists and of universal, the segment route \
                   {container} blank (alone and beside a failing segmentContainer) and outside the \
                   pattern, level spellings (4.1, -41, abc, 1e1, a leading space, 41., 4.1.2), a \
                   HEAD of the master playlist, an anonymous stream request, Static=true, and the 401 \
                   of an HLS request with no token coming first; and (2026-11-15) a \
                   stream.{container} blank (space, tab, beside a failing audioCodec, anonymous, an \
                   anonymous HEAD), outside the pattern (a$b, a leading space) and empty, the \
                   Stream. literal in another case, and a live stream file's container blank and \
                   outside the pattern (anonymous too), each beside a blank streamId, and its \
                   segment without the literal or empty after it. Not part of this \
                   row: merging these complaints with another \
                   parameter's binding failure into one body (gl-1672), the segments' {segmentId} \
                   binding, and the rest of each operation",
        date: "2026-11-15",
        divergences: &[],
    },
    VerifiedSubsystem {
        key: "transcode-arguments",
        upstream_file: "MediaBrowser.Controller/MediaEncoding/EncodingHelper.cs",
        upstream_method: "GetInputArgument",
        ferrofin: "The input and map arguments of a transcode job's ffmpeg command line, for \
                   the HLS and the progressive stream routes alike: every input option but the \
                   hardware device and decoder ones, and the -map arguments. \
                   apps/ferrofin-server planner.rs build_arguments' input and map blocks: \
                   fast_seek_ticks with the \
                   encoder's get_time_parameter (GetFastSeekCommandLineParameter and \
                   MediaEncoder.GetTimeParameter: the request's start, half a second later \
                   for an HLS remux of a video stream, clamped to [0, runtime - 5 s] when the \
                   runtime is known), the source's User-Agent and Referer headers \
                   (GetUserAgentParam, GetRefererParam), ferrofin-mediaencoding \
                   encoder/media_encoder.rs InputAnalysis::input_args \
                   (GetFfmpegAnalyzeDurationArg and GetFfmpegProbesizeArg: the source's own \
                   AnalyzeDurationMs, else the configured analyze duration, and the \
                   configured probe size, defaults 200M and 1G as upstream's), \
                   transcoding/segment_cleaner.rs \
                   input_sync_arguments, input_rate_arguments and input_source_arguments \
                   (GetInputModifier's -async and -vsync from AttachMediaSourceInfo's \
                   InputAudioSync and InputVideoSync, a wma, asf or local wtv container's \
                   among them, -re, -readrate and -readrate_catchup, an RTSP input's \
                   -rtsp_transport tcp+udp -rtsp_flags prefer_tcp, one -fflags joining \
                   +igndts, +ignidx and +genpts (GenPtsInput or a copied video), a video \
                   request's -f with its container's input format from \
                   encoding_helper/helper.rs input_format (GetInputFormat) while any \
                   hardware acceleration type but none is configured, the container for \
                   both being the job's input container from ferrofin-core \
                   media_source_manager.rs input_container (AttachMediaSourceInfo reads \
                   MediaSource.Container, which BaseItem.GetVersionInfo fills with the \
                   item's probed Container and the path's extension only when none was \
                   stored; an open live stream's own), upstream's \
                   never-assigned EncodingJobInfo.VideoType reading VideoFile for every job \
                   (a disc rip's -f concat comes later and wins), and the \
                   looping reconnect options) over \
                   transcoding/throttler.rs effective_input_protocol (AttachMediaSourceInfo's \
                   EncoderPath and EncoderProtocol), encoding_helper/hw/input_args.rs \
                   graphical_sub_canvas_size (GetGraphicalSubCanvasSize), concat_input with \
                   disc.rs concat_file_path and ensure_concat_config (GetInputArgument's DVD \
                   and Blu-ray branch, MediaEncoder.GenerateConcatConfig), media_encoder.rs \
                   source_input_argument and encoder/encoding_utils.rs get_input_argument \
                   (MediaEncoder.GetInputPathArgument and GetInputArgument, EncodingUtils: \
                   file: or bluray: before a path, a URL bare), encoding_helper/helper.rs \
                   external_stream_input_arguments and needs_external_subtitle_muxing (the \
                   rest of GetInputArgument: the external subtitle, read through the .idx \
                   beside a VobSub .sub, then the external audio; NeedsExternalSubtitleMuxing), \
                   then GetInputArgument's last rule, -noautoscale after the inputs whenever \
                   encoding_helper/hw/decoder.rs hardware_video_decoder names a decoder \
                   (GetHardwareVideoDecoder non-empty, a hardware decode feeding a software \
                   encoder included), \
                   and EncodingHelper::map_args with subtitle_map_args, \
                   negative_map_args_by_filters and find_index (GetMapArgs, \
                   GetNegativeMapArgsByFilters, FindIndex), over planner.rs plan's subtitle \
                   selection by SubtitleStreamIndex, for a video request alone \
                   (AttachMediaSourceInfo). Compared with those methods at the pinned v12.4 \
                   tag, and with DynamicHlsController.GetCommandLineArguments and \
                   EncodingHelper.GetProgressiveVideoFullCommandLine and \
                   GetProgressiveAudioFullCommandLine for where they go. The input options \
                   come in another order, which ffmpeg does not mind: each applies to the next \
                   -i. The software graphical-subtitle overlay maps its own graph's output, \
                   labelled [v], where upstream maps the raw video, cancels it with \
                   GetNegativeMapArgsByFilters and leaves the graph unlabelled; ffmpeg muxes \
                   the same streams in the same order either way. Not part of this row: the \
                   hardware device and decoder arguments (GetInputVideoHwaccelArgs, \
                   GetHardwareVideoDecoder) and the video filter graph, gl-1766; the video \
                   encoder and its arguments, gl-1765; the audio arguments, gl-1767; and the \
                   output and HLS muxer arguments, the -copyts of upstream's HLS command line \
                   among them, gl-1768",
        date: "2026-11-17",
        divergences: &[
            "The -f input format reads IsVideoRequest as is_input_video, the item's media \
             type, as the hardware device arguments do, so an audio-route request for a video \
             item with an accelerator configured passes it, where upstream's audio routes are \
             never video requests, and a video-route request for an audio item does not; \
             ffmpeg reads the file's own demuxer either way. Fix path: gl-1831.",
            "An AMF, VideoToolbox or RKMPP job passes no -noautoscale where its accelerator \
             names a decoder: it decodes in software here, so ffmpeg keeps its scaler. That \
             is the owner's hardware scope decision (CLAUDE.md, Current scope).",
            "A seeked HLS job copying the video passes -noaccurate_seek (not for a wtv \
             input), which upstream does not: Ferrofin's HLS output carries -output_ts_offset \
             in place of upstream's -copyts, and without it a re-encoded audio track would \
             start at the seek target while the copied video starts at the keyframe before \
             it, measured 3 s apart (gl-406). Deliberate divergence; the timing model it \
             rests on is to be recorded by gl-1768.",
            "Each external input is synced to the media file's clock with -isync 0 from \
             ffmpeg 5.1, which upstream does not pass: without -copyts each input's timeline \
             would start at its own first packet. Deliberate divergence; the timing model it \
             rests on is to be recorded by gl-1768.",
            "A bitmap sidecar (.sup, VobSub), embedded or burned in, is read from its start \
             rather than seeked like the media file (gl-1200), and on an ffmpeg before 5.1 \
             (or of a version that cannot be read) with -seek_timestamp 1 and -itsoffset \
             -<seek> in place of -isync (gl-929); helper.rs external_stream_input_arguments \
             records the measurements. Still open there: one clock on every ffmpeg, an owner \
             decision (gl-1214), a bitmap on screen at a seeked embed's start (gl-1215), and \
             a converted bitmap sidecar before ffmpeg 5.0 (gl-1221).",
            "A seeked progressive stream that embeds an external text sidecar, on an ffmpeg \
             before 5.1 (or of a version that cannot be read), reads a copy the server's \
             subtitle encoder cut to the start (FilterEvents), neither seeked nor synced and \
             mapped as 1:0, since that ffmpeg's output trim cannot cut the sidecar as \
             upstream's server-side cut does (gl-1199). Deliberate divergence.",
            "A sidecar whose path is a URL is read from the URL; upstream's muxing writes \
             -i file: before the URL, which ffmpeg cannot open. Jellyfin bug not ported \
             (gl-1722).",
            "An audio request maps the audio stream its job planned on (-vn, -map 0:<index> \
             and -map -0:s). Upstream's HLS and progressive audio command lines carry \
             no -map, so ffmpeg picks its default audio stream, which in a file with several \
             can be another one than the copy, bitrate and channel decisions read. Jellyfin \
             bug not ported.",
            "A video request for a file without a video stream seeks to its start, not half \
             a second later: upstream's IsVideoRequest gate also offsets such a request when \
             it asks for VideoCodec=copy, though the half second exists to land on a video \
             keyframe. Jellyfin bug not ported.",
            "A seek of 24 hours or more is written in total hours; upstream's TimeSpan format \
             hh:mm:ss.fff drops the whole days, so it seeks to that time of the first day. \
             Jellyfin bug not ported.",
            "A git build (ffmpeg version N-…) is read as the newest release whose libavutil \
             version its own has reached (encoder_validator.rs get_ffmpeg_version_internal), \
             where upstream's GetFFmpegVersionInternal reads every such build as 4.4, so \
             upstream passes -vsync -1 for a native-rate or wtv source on a current one, which \
             ffmpeg 9 rejects, and never -readrate (5.0) or -readrate_catchup (8.0). Jellyfin \
             bug not ported (gl-1796).",
            "The analyze duration and probe size are Ferrofin settings (ffmpeg_analyze_duration, \
             ffmpeg_probe_size) with upstream's defaults, clamped to 1M-600M and 1M-4G, and a \
             blank value is the default, so the flags are always passed; Jellyfin drops a flag \
             whose FFmpeg: value is empty and takes any value. Deliberate divergence (owner \
             decision 2026-11-12, gl-734).",
            "Header values, URLs and paths each reach ffmpeg as one argv element; upstream \
             puts a User-Agent, a Referer or a URL raw between quotes in its command string, \
             where a quote inside ends the value. A disc-rip playlist file whose path holds a \
             line break is refused, since the concat demuxer would read the rest as \
             directives; upstream writes it. Jellyfin bugs not ported.",
        ],
    },
    VerifiedSubsystem {
        key: "trickplay",
        upstream_file: "Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs",
        upstream_method: "RefreshTrickplayDataAsync",
        ferrofin: "Which trickplay sets a refresh discovers, imports, keeps, generates, replaces \
                   and prunes, for an item refresh (Replace included), the library scan and \
                   the Generate Trickplay Images task. ferrofin-core trickplay_manager.rs \
                   refresh_media (RefreshTrickplayDataAsync, with CanGenerateTrickplay and \
                   DiscoverExistingTrickplayAsync), refresh_widths, prepare_width, \
                   generate_sets and tile_and_save (RefreshTrickplayDataInternal), \
                   create_tiles and move_dir (CreateTiles), \
                   and scheduled_tasks/library.rs TrickplayImagesTask, compared with \
                   MediaBrowser.Providers/Trickplay/TrickplayImagesTask.cs (ExecuteAsync): \
                   each video gets its page's row, as upstream hands over the page's Video, \
                   and its stored streams from one read per page, where upstream's \
                   GetMediaStreams reads them at the video's turn. \
                   Also when the library scan refreshes a video: library_scan/trickplay.rs \
                   ScanTrickplay and library_scan.rs scan_item, compared with \
                   MediaBrowser.Providers/Trickplay/TrickplayProvider.cs (FetchInternal). \
                   Also the sets a changed video loses before its refresh: library_scan.rs \
                   scan_item and file_changed, item_data_cleanup.rs prune_extracted_data, \
                   external_data_manager.rs delete_external_item_data and \
                   trickplay_manager.rs prune_trickplay_data and delete_trickplay_data, \
                   compared with \
                   MediaBrowser.Providers/Manager/MetadataService.cs (BeforeSaveInternal), \
                   Emby.Server.Implementations/Library/ExternalDataManager.cs \
                   (DeleteExternalItemDataAsync) and TrickplayManager.cs \
                   (DeleteTrickplayDataAsync). \
                   How ffmpeg extracts the frames, on either path, is the \
                   trickplay-extraction row's; how many generations run at once is not part \
                   of this row. Also BaseItemDto.Trickplay: which media sources it lists \
                   and in what order, and the order of each source's widths. \
                   trickplay_manager.rs resolutions_for and \
                   get_trickplay_resolutions_batch (GetTrickplayResolutions), \
                   dto_service.rs trickplay_manifests and to_trickplay_manifest \
                   (GetTrickplayManifest: every source of GetMediaSources(false) but the \
                   IsRemote ones, through video_versions.rs local_media_source_ids over \
                   items_for_media_sources, GetAllItemsForMediaSources, in \
                   order_media_sources' order, and unfiltered by the user, as upstream), \
                   media_source_manager.rs is_remote_shortcut (GetVersionInfo's shortcut \
                   arm) and ferrofin-model dto/base_item.rs TrickplayManifest and \
                   serialize_trickplay_largest_first, compared with \
                   Emby.Server.Implementations/Dto/DtoService.cs (the Trickplay field), \
                   MediaBrowser.Controller/Entities/BaseItem.cs (GetMediaSources) and \
                   MediaBrowser.Controller/Entities/Video.cs (GetAllItemsForMediaSources)",
        date: "2026-11-14",
        divergences: &[
            "An explicit Replace does not delete the item's trickplay folder and rows first: \
             each selected width is generated in the work directory and swapped in, so the old \
             images serve until then and a failed Replace keeps them, and the rows and folders \
             of other widths go once every selected width has its row (with no width \
             selected, nothing is removed, where upstream's Replace empties the folder); with \
             extraction disabled it prunes first as upstream. A video narrower than two selected \
             widths generates their one set once. When the row save fails after the swap, the \
             new tiles stay if the width's stored row names their folder (upstream deletes them, \
             which would now leave the video without its old set). Deliberate divergence \
             (owner decision 2026-11-01, D7 of the Trickplay quality milestone, gl-1138).",
            "After WidthResolutions or FullChroma change since the last Generate Trickplay \
             Images run that finished (recorded in FerrofinMeta, and on the first boot after \
             upgrading), the next run checks every video: a set whose first tile's chroma \
             differs from the setting is regenerated and swapped in the same way, and a width \
             Ferrofin made sets at and the current settings do not select is removed once the \
             selected widths exist: one an earlier run selected, or one a library scan or an \
             item refresh made under settings other than those recorded (recorded before it is \
             made, which also checks again after a change back). A user-placed grid stays. The \
             run is recorded only when every video's sets match, so an interrupted run, or one \
             that could not replace a set, checks again. A run with no width selected is no \
             checking pass and records nothing (in a library with extraction on it makes and \
             removes nothing), so the next change still removes the \
             widths recorded before it. Upstream never replaces images unasked. Deliberate \
             divergence (owner decisions 2026-11-01, D3 and D5, gl-1138; gl-1178).",
            "A refresh or move of an item that a changed video's prune lands in removes its \
             trickplay folders and rows again once done; upstream's DeleteExternalItemDataAsync \
             does nothing about one in flight, so a generation from the old file that is still \
             running saves its set after the prune, and tiles it leaves without a row are \
             imported by the next refresh. Jellyfin bug not ported (gl-1694).",
            "Importing a width's folder that has no row counts only its .jpg tiles; upstream \
             counts every file in it, so a stray file there makes the import fail on its image \
             size. Jellyfin bug not ported.",
            "Importing a width's folder that has no row saves the row at the width the folder \
             was made at (the selected width, or the video's own when it is narrower, rounded \
             down to even), as a generated set's row is; upstream saves the selected width, \
             so when the two differ the row names a folder that does not exist (its tiles 404) \
             and gives the tiles the wrong aspect. Jellyfin bug not ported (gl-1246).",
            "TrickplayOptions has a Ferrofin-only FullChroma member, off by default and absent \
             from the JSON while off, which writes 4:4:4 tiles, from 4:4:4 frames on the \
             software path (see the trickplay-extraction row), instead of 4:2:0 (owner \
             decision 2026-11-01, D6, gl-1128).",
            "BaseItemDto.Trickplay lists each media source's widths largest first, every \
             time; upstream lists them in the order SQLite returns the rows, smallest first in \
             practice. Clients that show the first width listed (Wholphin, the Android TV and \
             Android apps) then get the sharpest set; jellyfin-web picks by screen size. \
             Deliberate divergence (owner decision 2026-11-03, gl-1226).",
            "A video holds one generation slot from its first width that generates until its \
             last width is done, so videos queued for a slot (a library scan's backlog) get each \
             video's sets together; upstream takes _resourcePool per width, under which a queued \
             video's first width would go before an earlier video's second. Order only: the \
             widths made, and the checks before each, are unchanged (gl-1227).",
            "A video's widths are each checked first (skipped, imported, or to generate, \
             with the write-access probe and the set recorded as made), and then the widths \
             to generate are extracted by one extract_trickplay_frame_sets call, which the \
             ffmpeg extractor makes from one decode (see the trickplay-extraction row), and \
             tiled and saved one by one; upstream checks, extracts, tiles and saves each width \
             before it checks the next. A width that fails, in its checks or in the run, is \
             logged and leaves the other widths as upstream's catch does. Order and the \
             number of decodes only: the widths made, and each width's checks, frames and \
             tiles, are unchanged (gl-1228).",
            "With the NonBlocking scan behaviour, the library scan detaches a video's refresh \
             once the item's save has written its row and its probe's streams, by id, so the \
             refresh reads both back as it starts instead of carrying the scan's copies; \
             upstream's TrickplayProvider starts it among the custom providers, from the \
             in-memory Video, before RefreshMetadata's final SaveItemAsync. Timing only: it \
             reads what the save and the subtitle downloads wrote, and Blocking is unchanged. \
             Owner decision (gl-233, option A).",
            "A set wider than 320 is made at a smaller tile grid than TileWidth x TileHeight \
             (options_at_width): each is multiplied by 320 / width, rounded down, at least 1, \
             so 640 gets 5 x 5 at the default 10 x 10 and a tile holds no more pixels than a \
             320 tile until an axis is down to 1. The grid names the set's folder and is \
             stored in its row, as upstream's configured grid is (RefreshTrickplayDataInternal, \
             CreateTiles), so a set at another grid is regenerated without a Replace, as \
             after a tile size change upstream. Upstream packs every width at the configured \
             grid. Deliberate \
             divergence (owner decision 2026-11-05, gl-1400): clients decode a whole tile to \
             show one preview, and Wholphin was seen decoding a 6400 x 3600 tile again on \
             every scrub. jellyfin-web and Wholphin read each width's own grid.",
        ],
    },
    VerifiedSubsystem {
        key: "video-stream-copy",
        upstream_file: "MediaBrowser.Controller/MediaEncoding/EncodingHelper.cs",
        upstream_method: "CanStreamCopyVideo",
        ferrofin: "Whether a transcode job copies the source video rather than re-encoding it, \
                   and what dynamic HDR metadata the copy strips. ferrofin-mediaencoding \
                   encoding_helper/helper.rs can_stream_copy_video (CanStreamCopyVideo) and \
                   its range block range_type_copy_ok, over EncodingJobInfo \
                   requested_range_types (GetRequestedRangeTypes); encoding_helper/bitstream.rs \
                   should_remove_dynamic_hdr_metadata, can_remove_dynamic_hdr_metadata (over \
                   the EncoderCapabilities seam's supports_bsf_with_option, \
                   IMediaEncoder.SupportsBitStreamFilterWithOption), is_dovi_removed, \
                   is_hdr10_plus_removed and the video arms of bit_stream_args \
                   (ShouldRemoveDynamicHdrMetadata, CanEncoderRemoveDynamicHdrMetadata, \
                   IsDoviRemoved, IsHdr10PlusRemoved, GetBitStreamArgs), the last three \
                   over the job's plan, dynamic_hdr_metadata_removal (upstream's, with the \
                   divergence below, strips_dovi_for_a_client_without_dovi); and the copy's -bsf:v \
                   gates, video_bit_stream_filters_apply, which apps/ferrofin-server \
                   planner.rs build_arguments reads (Jellyfin.Api/Controllers/\
                   DynamicHlsController.cs GetVideoArguments, \
                   EncodingHelper.GetProgressiveVideoArguments). Which client request leads \
                   to a transcode job, and the audio copy, are not part of this row",
        date: "2026-11-03",
        divergences: &[
            "Once a client has declared range types, a Dolby Vision stream with a base layer \
             (every Dolby Vision range type but profile 5's DOVI) copied to a client that \
             listed neither DOVI nor the stream's own range type has its RPU stripped \
             (bitstream.rs strips_dovi_for_a_client_without_dovi), where upstream plans no \
             removal for the base-layer fallbacks (DOVIWithHDR10 to an HDR10 client, \
             DOVIWithHLG to HLG, DOVIWithSDR to SDR), for HDR10+ beside Dolby Vision, for an \
             invalid configuration and for profile 7 to a client without HDR10, and copies the \
             RPU through (a Dolby Vision profile 8.1 AV1 title copied that way did not render \
             in jellyfin-web); an enhancement-layer player keeps upstream's HDR10+ removal. \
             Such a copy is refused, and the video transcoded, when ffmpeg has no Dolby Vision \
             strip filter for the stream's codec or the job's output takes no -bsf:v (a \
             progressive copy into anything \
             but MPEG-TS, a source already in Annex B form), and a copy that cannot be refused \
             (VideoCodec=copy, a user who may not transcode) on an ffmpeg without the filters \
             gets upstream's arguments. Deliberate divergence (owner decision 2026-10-31, \
             gl-737).",
        ],
    },
    VerifiedSubsystem {
        key: "metadata-refresh",
        upstream_file: "MediaBrowser.Providers/Manager/MetadataService.cs",
        upstream_method: "RefreshMetadata",
        ferrofin: "Which metadata and image providers an item's refresh runs, in which order \
                   and with what lookup, the Identify case first. A file-backed item: \
                   ferrofin-core library_scan.rs scan_item after ferrofin-providers \
                   refresh_plan.rs plan_item_refresh (GetProviders: every provider under \
                   ReplaceAllMetadata, a FullRefresh, or a first or required refresh at Default \
                   or above, else the change-monitored ones; for a locked item \
                   CanRefreshMetadata keeps the forced providers and RefreshWithProviders \
                   returns after them, so only the probe runs), scan_item's run_local gate \
                   (RefreshWithProviders runs \
                   no local reader when SearchResult is set (the NFO, and a book's embedded \
                   readers in enrich_from_file), nor under ReplaceAllMetadata for an item a \
                   saver rewrites (the NFO); a photo's PhotoProvider is a custom provider and \
                   runs whatever the options), ApplySearchResult \
                   (ResolverGuesses::pin_search's name and year and search_result_ids for the \
                   lookup; a season or an episode keeps none of its own ids, \
                   RefreshContext::identified, and its providers resolve the series by the \
                   chosen ids alone, ResolverGuesses::identified_series_ids: TheMovieDb's \
                   season and episode, TheTVDB's season and episode and OMDb's episode \
                   lookups, as SeriesProviderIds; its image providers read the series \
                   itself), remote_sources and provider_order \
                   (GetMetadataProvidersInternal: MetadataFetcherOrder, then IHasOrder, then \
                   registration, the plugins' sources among them; the chosen result's provider \
                   first, a stable OrderBy) and the artwork chain by FetcherPolicy::image_order \
                   (GetImageProvidersInternal's ImageFetcherOrder, GetNonLocalImageProviders' \
                   chosen provider first, every provider on an image FullRefresh or a first \
                   refresh, each filling what an earlier one left as RefreshImages' provider \
                   loop does). An album or an artist: library_scan/music.rs music_sources and \
                   music_images on the refresh's options, so the albums below an identified \
                   artist run the chosen provider first as well (MetadataRefreshOptions' copy \
                   keeps SearchResult through ValidateChildren), and music_fold_start's local \
                   reader skip. An item the provider manager refreshes (a box set, person, \
                   genre or studio, a virtual season or episode): ferrofin-providers \
                   provider_manager.rs \
                   refresh_item (the chosen ids stored first, RemoveImages under \
                   RemoveOldMetadata with ReplaceAllImages by local_images/merge.rs \
                   remove_images over the item's metadata folders and, when \
                   image_save.rs ImageFileSaver::beside_art says its art is saved beside it \
                   (IsSaveLocalMetadataEnabled), BesideArt::removes, as the scan's \
                   remove_item_images does too, a season's and an episode's \
                   providers given the chosen ids as their series' and the item's own ids \
                   cleared (ApplySearchResult runs whenever the item has a provider: an \
                   episode always, its forced ProbeProvider and TrickplayProvider, a \
                   path-less season only when a remote one runs), refresh_ordered_images \
                   with the chosen provider first). Compared with RefreshMetadata, \
                   ApplySearchResult, GetProviders, RefreshWithProviders, \
                   GetNonLocalImageProviders, ProviderManager.RefreshItem, \
                   GetMetadataProvidersInternal and GetImageProvidersInternal, and \
                   ItemImageProvider.RemoveImages and RefreshImages at the pinned v12.4 tag. What TheMovieDb's \
                   movie, series, season and episode providers answer is the \
                   metadata-providers row's; what the other providers answer (gl-1815) and how \
                   the answers merge field by field (gl-1813) are not part of this row.",
        date: "2026-11-19",
        divergences: &[
            "An album below an identified artist is looked up by its own ids, name and year; \
             upstream's ApplySearchResult gives every child but a season or an episode the \
             artist's result, its ids replacing the album's and its name and year the \
             album's, so the album is searched under the artist's name. Jellyfin bug not \
             ported.",
            "A Default scan also runs the remote providers of an item the backfill \
             heuristics pick, merging their answers fill-only; upstream runs no remote \
             provider on an unchanged Default refresh. Kept by owner decision 2026-09-24 \
             until the effect of removing the heuristics is understood.",
            "An item the provider manager refreshes (a box set, person, genre, studio, or \
             virtual season or episode) asks no plugin metadata source. Fix path: gl-1773.",
            "An item the provider manager refreshes runs no local image provider \
             (ValidateImages), except for a virtual season. Fix path: gl-1053.",
            "Replacing all images under RemoveOldMetadata, for an item whose art is saved \
             beside it, deletes a file outside its metadata folders only where \
             ImageFileSaver writes that item's art (the folder of its destinations, a \
             season's series folder), never its own file or a shared album cover, never \
             for a kind or image type the saver publishes nothing beside the media for \
             (a track, a photo, an extra, an episode's backdrop), and never in a \
             home-video library with photos enabled, where an image beside an album can \
             be one of its photos; upstream deletes every such image wherever it is stored. \
             Kept by gl-1741's data-safety rule (delete only where Ferrofin's saver \
             writes, nothing else in a media folder), which extends the owner decision \
             of 2026-10-17 on media-library writes to deletes; the owner has not yet \
             ruled on the narrower deletes.",
            "In a library with no metadata saver list and SaveLocalMetadata off, \
             ReplaceAllMetadata reads an item's existing NFO or a box set's existing \
             collection.xml; upstream's IsSaverEnabledForItem counts the saver enabled in \
             that library (isSavingMetadata) for an existing NFO, and always for a box set, \
             so it skips the file and rewrites it, where Ferrofin reads it and never rewrites \
             it (owner decision 2026-11-19).",
        ],
    },
    VerifiedSubsystem {
        key: "metadata-providers",
        upstream_file: "MediaBrowser.Providers/Plugins/Tmdb/Movies/TmdbMovieProvider.cs",
        upstream_method: "GetMetadata",
        ferrofin: "What TheMovieDb's metadata providers write for a movie, a series, a season \
                   and an episode. ferrofin-providers tmdb.rs details_for_locale \
                   (TmdbClientManager.GetMovieAsync and GetSeriesAsync: the language, the \
                   image languages and the append_to_response) and details_from_response (the \
                   names, overview, a movie's tagline and production countries, genres, \
                   studios by resolve_studios, release, air and end dates, status, vote \
                   average, certification by country, tags from keywords, trailers by \
                   youtube_trailers, the series \
                   runtime, home page, collection id and name, external ids and original \
                   language), credits_to_people with billed_cast, map_cast, \
                   map_aggregate_cast, crew_people and crew_person_type (MapCrewToPersonType, \
                   the MaxCastMembers and MaxCrewMembers caps, HideMissingCastMembers and \
                   HideMissingCrewMembers), series_people (the creators, outside the caps), \
                   season_details_for_language and season_details_from, \
                   episode_extras_for_language and episode_people_from (guest stars); \
                   ferrofin-core library_scan.rs fetch_tmdb_metadata (a pinned TMDb id, else \
                   find_tmdb_id's /find by an IMDb id, a series also by its TVDB id, else the \
                   parsed-name search and find_best_match, for a movie only without an IMDb \
                   id), apply_details (a movie's overview \
                   with its blank lines collapsed), fetch_tmdb_season \
                   with ferrofin-providers season.rs tmdb_answer (ImportSeasonName), and \
                   fetch_tmdb_episode with apply_tmdb_episode; for an episode the provider \
                   manager refreshes that has a path, episode.rs tmdb_answer (a path-less one \
                   is not asked, as TmdbEpisodeProvider returns nothing for \
                   IsMissingEpisode). Both read a multi-episode file's IndexNumberEnd range \
                   through episode.rs tmdb_episode_in_range; the scan's lookup has its stored \
                   row's range end, else its file name's (under Replace all metadata the file \
                   name's first), never its NFO's (MergeNewData passes ids only). Compared \
                   with TmdbMovieProvider, TmdbSeriesProvider (MapTvShowToSeries), \
                   TmdbSeasonProvider and TmdbEpisodeProvider \
                   GetMetadata, TmdbClientManager and TmdbUtils at the pinned v12.4 tag. \
                   TheMovieDb's box-set, person, missing-episode and similar-items providers \
                   and its image providers, the other providers (TheTVDB, OMDb, MusicBrainz, \
                   TheAudioDB; gl-1815) and the merge onto the item (gl-1813) are not part of \
                   this row.",
        date: "2026-11-15",
        divergences: &[
            "A movie, series, season or episode TMDb has no overview for answers the \
             refresh no Overview, so Identify → Apply and a Replace all metadata refresh \
             store none; upstream's provider answers TMDb's \"\", which those refreshes \
             store (RemoveOldMetadata skips the refill from the item). Fix path: gl-1843.",
            "A TMDb video with no key is dropped where upstream stores the dead link \
             https://www.youtube.com/watch?v=, and an episode's trailer URL already added is \
             compared exactly where upstream's AddTrailerUrl ignores case (YouTube keys are \
             case-sensitive). Jellyfin bugs not ported.",
            "A blank content rating is skipped; upstream takes a blank own-country series \
             rating (storing a bare country prefix such as GB-) and lets a blank US one \
             block its first-entry fallback. Jellyfin bug not ported.",
            "An episode is looked up in aired order whatever the series' display order; \
             upstream maps an absolute, DVD, digital, story-arc, production, TV or \
             original-air-date order through TMDb's episode group. Fix path: gl-1784.",
            "An episode with no season number is not looked up; upstream looks it up in \
             season 1 (TmdbEpisodeProvider.cs:99), with its resolver's season-1 default for \
             a loose episode, which Ferrofin's planner lacks too. Fix path: gl-1846.",
            "A date-only value (premiere, end and air dates) is stored as midnight UTC; \
             upstream stores the server's local midnight. Pending an owner decision \
             (gl-490).",
            "A season's or an episode's series TMDb id, and a path-less item's TMDb id, \
             are read with surrounding whitespace or a sign; upstream's TryParseTmdbId \
             reads digits only. Fix path: gl-1774.",
            "A series whose stored TMDb id is not plain digits is looked up through /find \
             and the name search; upstream's series provider parses it as an integer \
             (pinning a padded or signed id) and returns no metadata when it is not one. \
             Fix path: gl-1774.",
            "A vote average of 0 is stored as no community rating, and a series whose \
             episode_run_time is empty as no runtime; upstream stores 0 for both, which \
             keeps a later provider (OMDb, TheTVDB) from filling them. Jellyfin bug not \
             ported.",
        ],
    },
];

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

/// The code path the two scheduled-task read rows share after their handler:
/// `ScheduledTaskHelpers.GetTaskInfo` over the task's worker.
macro_rules! scheduled_task_info {
    () => {
        "Registration::to_info (ScheduledTaskHelpers.GetTaskInfo: Name, Description and \
         Category through GetLocalizedString, in the request's CurrentUICulture, where the task \
         localizes (every core task but RefreshGuide and TmdbRefreshUpcomingEpisodes, whose \
         upstream text is literal; an extension or WASM task shows its own text); State; \
         CurrentProgress only while a run is live, as ExecuteInternal nulls it after; Id, the \
         MD5 of the C# task type (a WASM plugin task's: of Ferrofin.ScheduledTasks.<key>); \
         IsHidden, which can be true only for RefreshGuide (no tuner host) and \
         RefreshInternetChannels (no channel), as GetTaskInfo reads it only from an \
         IConfigurableScheduledTask and no other configurable task hides; Key; \
         Triggers, the saved ones else the task's defaults, saved meaning Ferrofin's trigger \
         store, else an adopted Jellyfin's {ConfigurationDirectoryPath}/ScheduledTasks/\
         {new Guid(Id)}.js read as LoadTriggerSettings reads a file Jellyfin wrote, null \
         entries dropped; LastExecutionResult, the history store's, else an adopted Jellyfin's \
         {DataPath}/ScheduledTasks/{new Guid(Id)}.js read once at registration as the \
         LastExecutionResult getter reads a file Jellyfin wrote, a missing time or status as its \
         default, an empty or malformed file skipped). The registry also holds the compiled-in \
         extensions' tasks and any WASM plugin's (the plugin tiers, docs/EXTENSIONS.md), as \
         upstream lists an installed plugin's: a disabled plugin's stay listed, their runs \
         no-ops, and FERROFIN_DISABLE_EXTENSIONS leaves the compiled-in ones out"
    };
}

/// The code path the item image rows share after their handler.
macro_rules! item_image_tail {
    () => {
        "authorize_image_item (GetItemById(itemId, User.GetUserId()): a signed-in user's \
         item must be visible to them, anonymous and API-key callers are not checked) → \
         serve_item_image (BaseItem.GetImageInfo: get_chapter_image for a Chapter, else \
         select_image's GetImages(type).ElementAtOrDefault(index), so a negative index is a \
         404) → serve_image_file (GetImageInternal, GetImageResult: local_image for \
         ConvertImageToLocal; ProcessImage on every request; the two DLNA headers; Age, \
         Vary and the disposition on every answer; is_fresh's 304 for an If-None-Match \
         naming the tag or an If-Modified-Since at or after ProcessImage's date, both \
         skipped under no-cache (a Cache-Control value that is exactly no-cache, as \
         StringValues.Contains), with the type and that date's Last-Modified; then \
         PhysicalFile's own If-Modified-Since (ignored when in the future) and \
         If-Unmodified-Since against the served file, with that file's Last-Modified, and \
         none of its range processing) → \
         ferrofin-drawing processor.rs process_image (ImageProcessor.ProcessImage) → \
         image_encoder.rs encode_image and composite (SkiaEncoder.EncodeImage: the blur, \
         backgroundColor and foregroundLayer steps) → limits.rs ImageMemory. Measured on \
         12.4 with a BMP and a transparent PNG primary: no transform, quality, Range, \
         If-None-Match, If-Modified-Since (past, the file's own date and future, with and \
         without no-cache), Cache-Control spellings, If-Unmodified-Since, \
         If-Match, backgroundColor (valid and not), foregroundLayer, blur from 1 to \
         2147483647, a hex imageIndex and a malformed percentPlayed; the images match in \
         format and size, and in pixels within rounding and the blur's box approximation \
         of Skia's Gaussian (with its Decal edges and premultiplied JPEG), not in bytes \
         (the image crate, not Skia)"
    };
}

/// The divergences the six item image rows share (GET and HEAD, plain, indexed
/// and positional).
const ITEM_IMAGE_DIVERGENCES: &[&str] = &[
    "percentPlayed and unplayedCount are bound but not drawn, and not passed to the \
     processor: a request naming a percentPlayed between 0 and 100 or a positive \
     unplayedCount (the values GetImageInternal keeps) and nothing else serves the stored \
     file where upstream re-encodes it with the overlay. Fix path: gl-806.",
    "foregroundLayer is read as an invariant decimal; upstream's double.TryParse reads it \
     in the server's culture (in a comma-decimal culture, 0,5 is a half).",
    "If-Modified-Since is compared with ProcessImage's date in UTC; upstream's \
     DateTime.TryParse reads the header as the server's local time, so outside UTC its \
     own check is off by the offset (PhysicalFile's, against the file, is not). Jellyfin \
     bug not ported.",
    "A remote image that cannot be downloaded is a 404; upstream's ConvertImageToLocal \
     throws, a 500. Jellyfin bug not ported.",
    "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
];

/// The code path the six `/{kind}/{itemId}/Similar` rows share: one handler
/// serves every alias, as `LibraryController.GetSimilarItems` does.
macro_rules! similar_items_path {
    () => {
        "handlers/similar.rs similar_items, one body for the six aliases (excludeArtistIds \
         through parse_csv_uuids, fields through parse_csv_enums_lenient, limit; userId \
         through resolve_user_opt: RequestHelpers.GetUserId, then GetUserById, so an id naming \
         no account runs with no user; require_visible_item's 404 for a non-nil itemId the \
         user cannot see, GetItemById(itemId, user); resolve_similar_seed's root folder for a \
         nil one, a kind no provider serves, so an empty page; DtoOptions { Fields = fields } \
         plus ProviderIds, which GetSimilarItemsAsync adds to the options the controller then \
         projects with) → ferrofin-core similar_items_manager.rs get_similar_items (the \
         Episode and by-name-but-not-MusicArtist short-circuit, kinds.rs \
         supports_similarity; limit ?? 50; similarity_plan: GetLibraryOptions(item) through \
         owning_library, the type's TypeOptions by class name, the remote providers it ticked \
         in SimilarItemProviders and the always-enabled local scorer ordered as one list by \
         SimilarItemProviderOrder, else SimilarItemProviders, an unlisted name last \
         (GetConfiguredSimilarProviderOrder); the provider loop's limit check, ExcludeItemIds, \
         the id-and-presentation-key claim and CalculateScore, and the loop's catch \
         (push_local_results warns with the provider name and item id when the local scorer \
         fails, and the remote providers still run); local_filter per kind: \
         MovieSimilarItemsProvider for Movie and Trailer, SeriesSimilarItemsProvider, \
         MusicAlbum-, MusicArtist- and AudioSimilarItemsProvider (AudioBook through Audio) \
         and LiveTvProgramSimilarItemsProvider by IsMovie and IsSeries; local_similar's score \
         pass, similar_items_repository.rs weighted_similar_items (genre 10, tag and studio 5, \
         director 50, actor and guest star 15, the seed's people distinct), its one filter \
         query, score_filter_query (Movie, plus Trailer and LiveTvProgram under \
         EnableExternalContentInSuggestions; IsMovie; IsPlayed = false; the user's access; \
         TranslateQuery without ApplyGroupingFilter, so never grouped by presentation key), \
         and phase 3, best_scoring_per_key (by score, the primary first on a tie, then \
         DistinctBy the raw PresentationUniqueKey, every keyless row one key); \
         flat_similar for the other kinds (the seed's Genres and Tags, both required when both \
         are set, Random order, ExcludeArtistIds for the music kinds, and for a movie-like \
         programme EnableGroupByMetadataKey, so item_repository.rs grouped_by_metadata_key \
         reads 4 more); remote_results (the \
         reference cache at {cache}/{provider}-similar-{type}/{id:N}.json, batches of 20, the \
         500-reference cap, the cache write) and resolve_remote_references (the best \
         reference per provider id, one user-scoped query of the seed's kind, the highest \
         score per presentation key); the stable score sort, then Take; no \
         FilterByLibraryAccessAsync pass, as every query is already user-scoped) → \
         ferrofin-providers similarity.rs (TmdbMovieSimilarProvider, \
         TmdbSeriesSimilarProvider, ListenBrainzSimilarArtistProvider) → dto_service.rs \
         get_base_item_dtos with its IsVisible pass for a user; QueryResult(0, the item count \
         before that pass, the DTOs)"
    };
}

/// What `GET /Trailers` does unlike upstream, beyond the subsystems its row
/// refs. Every entry but the first two is `GetItems`' own, so a `GET /Items`
/// row would carry them too.
const TRAILERS_DIVERGENCES: &[&str] = &[
    "Upstream answers 500 to every request: GetTrailers calls an ItemsController the service \
     container built, which has no HttpContext, so GetItems' User.GetIsApiKey() throws a \
     NullReferenceException (seen on 12.4). Jellyfin bug not ported: Ferrofin answers what that \
     GetItems call is written to, through GET /Items' own binding, so an indexNumber (a member \
     GetTrailers does not take) still binds and one that is no number answers 400.",
    "studios, artists and albums split on pipes, as GetItems binds them and as the published \
     OpenAPI documents them for this route; GetTrailers' own binder splits them on commas. \
     Jellyfin bug not ported.",
    "maxOfficialRating can lower a rating-limited user's ceiling but never raise it: the \
     stricter of the request's and the user's applies, where upstream's replaces the user's and \
     an unrecognized rating clears it. Jellyfin bug not ported (gl-809).",
    "An albums name no album has matches nothing, where upstream drops it, so a list of only \
     unknown names filters nothing there. Jellyfin bug not ported.",
    "A searchTerm is ranked and paged by the repository's search (relevance, then SortName), \
     not by the search providers' scores with their 3 × limit candidates paged in memory, and \
     explicit ids are intersected with the term rather than joined to its matches. Fix path: \
     gl-450.",
    "An API key without a userId runs at the user root, so a non-recursive request lists every \
     trailer where upstream's parent is the root folder, whose direct children hold none; a \
     parentId naming an item that is not a folder stays the parent, where upstream answers at \
     the user root; and contradictory filters answer 400 before the parent's visibility is \
     checked. Fix path: gl-1851.",
];

/// The divergences the six `/{kind}/{itemId}/Similar` rows share.
const SIMILAR_ITEMS_DIVERGENCES: &[&str] = &[
    SIMILAR_SCORE_WINDOW_DIVERGENCE,
    SIMILAR_ALTERNATE_VERSIONS_DIVERGENCE,
    "A reference-cache entry that exists but cannot be read (a directory at its path, an \
     entry or cache directory the server may not read) warns once with its path and is \
     fetched again from the provider. Upstream reads a directory, or an entry under a \
     directory it may not search, as a silent miss, and an unreadable entry file throws an \
     UnauthorizedAccessException, not an IOException, so the provider's catch warns and its \
     results drop out of the response. Jellyfin bug not ported: a silent miss hides a disk \
     problem while every request pays a provider call.",
    "A library adopted from Jellyfin 10.11, or saved before similarity providers were \
     configurable, keeps the empty SimilarItemProviders that Jellyfin 12's \
     EnableLocalSimilarityProviders routine fills with Local Genre/Tag, so a client that then \
     ticks a remote provider without writing SimilarItemProviderOrder ranks the local scorer \
     after it, where upstream ranks it first (jellyfin-web writes the order, which both \
     servers then follow). Fix path: gl-1670.",
    "A movie or trailer seed's page is refilled past the rows the id-and-key claim drops \
     (another version of the seed: a copy in another library, or an alternate seed's \
     primary; a key differing only in case), from the score pass's oversample; upstream's \
     movie provider hands over its limit rows, so the claim's drops leave those places \
     short, or to the next provider when one is ranked after it. Jellyfin bug not ported.",
    "A request with no user (an API key naming none, or an administrator naming a userId \
     with no account) scores a movie or trailer seed locally without the played filter; \
     upstream's movie provider throws on IsPlayed without a user (BuildIsPlayedFilter \
     dereferences it), which the manager logs as a provider failure, so it answers the \
     remote providers' matches alone. Jellyfin bug not ported.",
    "A limit above 715827882 overflows upstream's limit * 3 to a negative Take, so its movie \
     provider answers nothing; the oversample saturates here. Jellyfin bug not ported.",
    "A malformed id in excludeArtistIds is a 400; upstream's \
     CommaDelimitedCollectionModelBinder drops it. Fix path: gl-884.",
    "The 400 (a value the query binder cannot convert), 403 (another user's id without the \
     administrator role) and 404 (an item the user cannot see) bodies are Ferrofin's error \
     shape, not upstream's. Fix path: gl-666.",
];

/// How a movie or trailer seed's score pass differs from upstream's, for the
/// similar-items rows and the recommendations row (whose baselines all take it).
const SIMILAR_SCORE_WINDOW_DIVERGENCE: &str = "A movie or trailer seed's score pass (every \
     recommendation baseline's too) keeps its top limit * 3 among the result kinds without \
     the excluded ids, cuts a tie at that boundary by Id (the owner's tie rule, gl-710), and \
     matches genres, tags and studios by stored value; upstream's keeps the top limit * 3 of \
     every item it scored (an episode or series sharing an actor included) before it filters \
     kinds and excluded ids, cuts a tie in the order the scores were added, and matches clean \
     values, so its page can hold fewer or other movies. Fix path: gl-1786.";

/// The 12.1 alternate-version rule the score pass still lacks (gl-392).
const SIMILAR_ALTERNATE_VERSIONS_DIVERGENCE: &str = "The score pass scores an alternate \
     version whose primary is in the same library, which upstream's hiddenVersionIds leaves \
     out (12.1), so such versions take places in the limit * 3 window before the filter \
     query drops them. Fix path: gl-392.";

/// The code path the four legacy `HlsSegmentController` rows share after their
/// handler, from the resolver to the response.
macro_rules! legacy_hls_tail {
    () => {
        "ferrofin-mediaencoding manager.rs request_progress_for_file on the job's file, the \
         playlist file found on the video segment and the served file otherwise \
         (GetFileResult's OnTranscodeBeginRequest(playlistPath), before the file is opened) \
         → the file's existence on the blocking pool (PhysicalFile's FileNotFoundException, \
         a 404, whose dropped observer still restamps the job as OnTranscodeEndRequest's \
         PingTimer does) → served_file_response (GetStaticFileResult, a PhysicalFile with \
         EnableRangeProcessing: serve_static_file's Range, with the type \
         MimeTypes.GetMimeType(file) through mime_for_extension) with the response body \
         holding the observer, so the end of the download, or a client hanging up, releases \
         it (Response.OnCompleted's OnTranscodeEndRequest)"
    };
}

/// What the `static-file-ranges` row leaves to gl-1823, for the item file rows
/// served by `serve_physical_file`.
const STATIC_FILE_CONDITIONAL_DIVERGENCE: &str = "Conditional requests are answered as \
     tower-http's ServeFile answers them: an ETag on every answer, If-Match and If-None-Match \
     honoured, an If-Modified-Since or If-Unmodified-Since after now counted, and a 416 with no \
     Content-Type; upstream's PhysicalFile sends no ETag, evaluates neither tag header nor a \
     future date, and types its 416. Fix path: gl-1823.";

/// How the four legacy `HlsSegmentController` rows' error bodies differ.
const LEGACY_HLS_BODIES: &str = "The 404 bodies are Ferrofin's error shape, where upstream \
     sends its exception middleware's text/plain \"Error processing request.\" for a missing \
     file and, on the video segment, the JSON string \"Hls segment not found.\". Fix path: \
     gl-666.";

/// The keep-alive the legacy audio segment rows take where upstream takes none.
const LEGACY_HLS_AUDIO_KEEP_ALIVE: &str = "The job whose playlist's file stem prefixes the \
     segment's is kept active for the download, which upstream's action never does. A \
     deliberate, response-invisible superset: it only stops the idle reaper from killing a \
     transcode a client is reading.";

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
///   checks every row dated before [`UPSTREAM_PIN_DATE`] is listed there,
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
               (TVSeriesManager; keys_scope: GetItemById's 400 for a nil parentId, after \
               the seriesId return; a box set or playlist parent as DescendantOfId) → \
               next_up_service.rs (NextUpService, top_parent_ids → item_repository.rs \
               view_top_parent_ids; linked_container_scope: SetTopParentIdsOrAncestors' \
               LinkedChildren test, DescendantQueryHelper's set, AddUserToQuery's \
               scope_to_user_libraries)",
        date: "2026-11-15",
        divergences: &[
            "TVSeriesManager.GetPreferredVersion (continue in the alternate version the user \
         has been watching, matched by media-source name) is not ported: the pick is always \
         the primary episode row. Un-defer path: v12's Video.GetMediaSourceName \
         common-prefix naming is ported (video_versions::media_source_name); swap by that \
         name in tv_series_manager (see the TODO there).",
            "parentId naming a non-view item other than a box set or playlist: v12 routes it \
         to AncestorIds, which the keys statement ignores, so the answer is always empty; \
         Ferrofin scopes by TopParentId = that item (a physical library folder answers, \
         anything else is empty as upstream). Jellyfin bug not ported.",
            "parentId naming a box set or playlist with linked children: v12.4's \
         SetTopParentIdsOrAncestors sets DescendantOfId, which NextUpService's keys \
         statement ignores, so AddUserToQuery scopes it to every library the user has \
         (those excluded from Latest included) and the answer is that whole Next Up; \
         Ferrofin answers the series under its descendant set (the episodes it links, and \
         those of a series or season it links) within those libraries, as upstream's \
         comment intends. Owner decision 2026-11-02 (gl-36): Jellyfin bug not ported. One \
         with no linked children is empty in both.",
            "The 400 (an API key with no userId, a nil parentId, or a value the query binder \
         cannot convert), 403 (another user's id without the administrator role) and 404 (a \
         userId naming no account) bodies are Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Albums/{itemId}/Similar",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetSimilarItems",
        ferrofin: similar_items_path!(),
        date: "2026-11-15",
        divergences: SIMILAR_ITEMS_DIVERGENCES,
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Artists/{itemId}/Similar",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetSimilarItems",
        ferrofin: similar_items_path!(),
        date: "2026-11-15",
        divergences: SIMILAR_ITEMS_DIVERGENCES,
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Items/{itemId}/Similar",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetSimilarItems",
        ferrofin: similar_items_path!(),
        date: "2026-11-15",
        divergences: SIMILAR_ITEMS_DIVERGENCES,
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Movies/{itemId}/Similar",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetSimilarItems",
        ferrofin: similar_items_path!(),
        date: "2026-11-15",
        divergences: SIMILAR_ITEMS_DIVERGENCES,
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Trailers",
        upstream_file: "Jellyfin.Api/Controllers/TrailersController.cs",
        upstream_method: "GetTrailers",
        ferrofin: "handlers/trailers.rs get_trailers (GetTrailers: GetItems' parameters bound \
                   with GetItems' table, includeItemTypes replaced by [Trailer] and indexNumber \
                   by none; enableTotalRecordCount and enableImages default true) → \
                   handlers/items.rs get_items, the GetItems port compared with it: \
                   resolve_user_or_api_key and effective_user_id, whitelists_tags (AllowedTags \
                   adds Tags), ItemsQuery::dto_options (AddAdditionalDtoOptions), \
                   items_query_options, official_rating_score, resolve_name_filters, \
                   apply_media_stream_filter_rules, apply_location_types, apply_filters, \
                   check_parent_visibility (GetParentItem, IsVisible, and a UserView parent's \
                   UserViewBuilder through LibraryManager apply_user_view_builder, \
                   ferrofin-core library_manager/user_view_builder.rs), redirect_container_browse \
                   (the BoxSet and Playlist re-root), list_playlists_folder_as_playlists, \
                   apply_library_browse_defaults (recursive ??= true for a typed library \
                   browse), apply_albums_by_artist_order, order_search_by_sort_name, the \
                   UserRootFolder and HasFilters dispatch, retain_visible_ids, \
                   sort_items_by_request (Folder.GetItems' request order) → LibraryManager \
                   query_items and DtoService get_base_item_dtos, or get_linked_children_dtos \
                   for a box set's or playlist's own non-recursive listing",
        date: "2026-11-15",
        divergences: TRAILERS_DIVERGENCES,
        refs: &["access-filtering", "images", "paging-sort", "user-data"],
    },
    Verified {
        method: "get",
        path: "/Trailers/{itemId}/Similar",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetSimilarItems",
        ferrofin: similar_items_path!(),
        date: "2026-11-15",
        divergences: SIMILAR_ITEMS_DIVERGENCES,
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Shows/{itemId}/Similar",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetSimilarItems",
        ferrofin: similar_items_path!(),
        date: "2026-11-15",
        divergences: SIMILAR_ITEMS_DIVERGENCES,
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Movies/Recommendations",
        upstream_file: "Jellyfin.Api/Controllers/MoviesController.cs",
        upstream_method: "GetMovieRecommendations",
        ferrofin: "handlers/movies.rs get_movie_recommendations (userId through \
               resolve_user_opt: RequestHelpers.GetUserId, then GetUserById; parentId ?? \
               Guid.Empty through require_scope_id_visible; fields; categoryLimit = 5 and \
               itemLimit = 8 by default; GetBaseItemDtos' IsVisible pass per category as one \
               filter_visible_items pass over every category's distinct items, a category it \
               empties kept with no items, then one get_base_item_dtos pass over what is \
               left, split back per category in order) → ferrofin-core \
               similar_items_manager.rs get_movie_recommendations \
               (GetMovieRecommendationsAsync: recently_played_query, played movies by \
               DatePlayed then Random, Limit 7, and liked_query, IsFavoriteOrLiked movie kinds \
               in Random order, Limit 10, without the recently played, both under ParentId and \
               EnableGroupByMetadataKey, so item_repository.rs grouped_by_metadata_key reads 4 \
               more; similar_items_repository.rs people_names_of over the first 6 played, \
               GetPeopleNamesByItems in ListOrder per item, distinct, with equal ListOrders \
               on two films in ItemId order, the order SQLite returns for upstream's \
               statement; each baseline capped to categoryLimit and scored by \
               similar_category through local_similar, the \
               MovieSimilarItemsProvider path of the similar-items rows for every baseline, \
               whatever its kind, with the user and Limit = itemLimit, no remote provider; \
               next_person_category, the lazy GetPersonRecommendations: Person = name, \
               Limit = itemLimit + 2 grouped by metadata key, PersonTypes \
               Director for the director stream and none for the \
               actor stream, IsMovie, IsPlayed = false, DistinctBy the IMDb id, Take(itemLimit), \
               CategoryId = name.GetMD5(); round_robin_categories: recently played, recently \
               played, liked, liked, directors, actors until categoryLimit or a pass adds \
               nothing, then a stable OrderBy RecommendationType)",
        date: "2026-11-15",
        divergences: &[
            SIMILAR_SCORE_WINDOW_DIVERGENCE,
            SIMILAR_ALTERNATE_VERSIONS_DIVERGENCE,
            "Each baseline is scored and access-filtered on its own and fills its category \
         from its own limit * 3 window; upstream's IBatchLocalSimilarItemsProvider scores \
         every baseline of a stream in one pass, filters the union of their windows once and \
         fills each category from any candidate in that union its baseline scored, so a \
         category the filter left short can be longer upstream. Fix path: gl-1357.",
            "An itemLimit above 715827882 overflows upstream's limit * 3 to a negative Take, so \
         every similar-to category is empty; the oversample saturates here. Jellyfin bug not \
         ported.",
            "A similar-to category leaves out a film sharing its baseline's presentation key \
         (another copy of the baseline); upstream's batch provider excludes only the \
         baseline's own id and can recommend that copy under its own name. Jellyfin bug not \
         ported.",
            "A request with no user (an API key naming none, or an administrator naming a \
         userId with no account) answers an empty list; upstream's recently-played query \
         throws on IsPlayed without a user (BuildIsPlayedFilter dereferences it), a 500. \
         Jellyfin bug not ported.",
            "The 400 (a value the query binder cannot convert) and 403 (another user's id \
         without the administrator role) bodies are Ferrofin's error shape, not upstream's. \
         Fix path: gl-666.",
        ],
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Artists",
        upstream_file: "Jellyfin.Api/Controllers/ArtistsController.cs",
        upstream_method: "GetArtists",
        ferrofin: "handlers/artists.rs get_artists (items.rs resolve_user_or_api_key) → \
               handlers/by_name.rs (ByNameListQuery check_min_community_rating, \
               artists_query, resolve_by_name_ids, \
               project_query_result) → ferrofin-core item_repository.rs get_artists \
               (item_values_with_counts, item_counts_by_clean_name); \
               its scope rewrite top_parent_or_ancestor_scope \
               (LibraryManager.SetTopParentOrAncestorIds) compared at v12.4 on 2026-11-13",
        date: "2026-10-25",
        divergences: BY_NAME_DIVERGENCES_ARTISTS,
        refs: &["images", "access-filtering", "paging-sort"],
    },
    Verified {
        method: "get",
        path: "/Artists/AlbumArtists",
        upstream_file: "Jellyfin.Api/Controllers/ArtistsController.cs",
        upstream_method: "GetAlbumArtists",
        ferrofin: "handlers/artists.rs get_album_artists (items.rs resolve_user_or_api_key) → \
               handlers/by_name.rs (check_min_community_rating, artists_query, \
               resolve_by_name_ids) → ferrofin-core \
               item_repository.rs get_album_artists (item_values_with_counts); \
               its scope rewrite top_parent_or_ancestor_scope \
               (LibraryManager.SetTopParentOrAncestorIds) compared at v12.4 on 2026-11-13",
        date: "2026-10-25",
        divergences: BY_NAME_DIVERGENCES_ARTISTS,
        refs: &["images", "access-filtering", "paging-sort"],
    },
    Verified {
        method: "get",
        path: "/Genres",
        upstream_file: "Jellyfin.Api/Controllers/GenresController.cs",
        upstream_method: "GetGenres",
        ferrofin: "handlers/genres.rs get_genres (by_name.rs ValueListQuery::genres: the \
               declared parameters only; items.rs resolve_user_or_api_key; \
               music/musicvideos library parent → get_music_genres) → handlers/by_name.rs → \
               ferrofin-core item_repository.rs get_genres / get_music_genres \
               (item_values_with_counts, \
               top_parent_or_ancestor_scope: LibraryManager.SetTopParentOrAncestorIds)",
        date: "2026-11-13",
        divergences: BY_NAME_DIVERGENCES_GENRES_STUDIOS,
        refs: &["images", "access-filtering", "paging-sort"],
    },
    Verified {
        method: "get",
        path: "/MusicGenres",
        upstream_file: "Jellyfin.Api/Controllers/MusicGenresController.cs",
        upstream_method: "GetMusicGenres",
        ferrofin: "handlers/music_genres.rs get_music_genres (by_name.rs \
               ValueListQuery::genres: the declared parameters only; items.rs \
               resolve_user_or_api_key) → handlers/by_name.rs → ferrofin-core \
               item_repository.rs get_music_genres (item_values_with_counts, \
               top_parent_or_ancestor_scope: LibraryManager.SetTopParentOrAncestorIds)",
        date: "2026-11-13",
        divergences: BY_NAME_DIVERGENCES_GENRES_STUDIOS,
        refs: &["images", "access-filtering", "paging-sort"],
    },
    Verified {
        method: "get",
        path: "/Studios",
        upstream_file: "Jellyfin.Api/Controllers/StudiosController.cs",
        upstream_method: "GetStudios",
        ferrofin: "handlers/studios.rs get_studios (by_name.rs ValueListQuery::studios: \
               the declared parameters only; items.rs resolve_user_or_api_key) → \
               handlers/by_name.rs → ferrofin-core item_repository.rs get_studios \
               (item_values_with_counts, \
               top_parent_or_ancestor_scope: LibraryManager.SetTopParentOrAncestorIds)",
        date: "2026-11-13",
        divergences: BY_NAME_DIVERGENCES_GENRES_STUDIOS,
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/UserItems/Resume",
        upstream_file: "Jellyfin.Api/Controllers/ItemsController.cs",
        upstream_method: "GetResumeItems",
        ferrofin: "handlers/items.rs get_resume_items (active_session_item_ids → \
               ferrofin-core session_manager.rs get_session_summaries) → ferrofin-core \
               item_repository.rs get_items → translate_query.rs (push_resumable_predicate, \
               push_order_expression DatePlayed)",
        date: "2026-10-30",
        divergences: RESUME_DIVERGENCES,
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Sessions",
        upstream_file: "Jellyfin.Api/Controllers/SessionController.cs",
        upstream_method: "GetSessions",
        ferrofin: "handlers/session.rs get_sessions (items.rs effective_user_id) → \
               ferrofin-core session_manager.rs get_sessions (shared_device_control_for, \
               retain_accessible_session_devices → device_manager.rs can_access_devices, \
               to_dto → session_info_to_dto) (SessionManager.GetSessions, CanAccessDevice, \
               ToSessionInfoDto), the users' permissions and EnabledDevices read from the policy \
               cache (user_entity_ext.rs cached_permission, cached_preference; the User \
               upstream holds) and a controllableByUserId naming the caller answered from the \
               caller's row; the NowPlayingItem it shows is \
               the report's own Item as sent, else the library item on_playback_start and \
               on_playback_progress name (NowPlayingItem::of on the report's row, \
               kept_now_playing_item, shown_reported_item; UpdateNowPlayingItem), built by the \
               first listing and then copied (shown_now_playing_item → now_playing_item → \
               MediaSourceManager::get_media_source on that row, get_item_info; GetMediaSource, \
               GetItemInfo), and cleared by on_playback_stopped (RemoveNowPlayingItem); the \
               PlayState and PlaylistItemId are the last report's (update_now_playing; \
               UpdateNowPlayingItem, MediaSourceId the item's own when none is reported), the \
               stop keeps a reported NowPlayingQueue and resets the PlayState \
               (OnPlaybackStopped, RemoveNowPlayingItem); TranscodingInfo is cleared by a report \
               that does not transcode, by the stop and by a transcode that fails to start \
               (set_device_transcoding_info, on the device's session active last, as \
               ReportTranscodingInfo's Sessions order; mediaencoding manager.rs start_ffmpeg → \
               on_job_failed_to_start; ClearTranscodingInfo, OnTranscodeFailedToStart), whose \
               reported PlayMethod handlers/playstate.rs validate_play_method keeps only while \
               the play session transcodes (ValidatePlayMethod); its TranscodeReasons are the \
               job's (manager.rs transcoding_info; ReportTranscodingProgress's \
               state.TranscodeReasons): the request's TranscodeReasons as ferrofin-model \
               parse_transcode_reasons reads them (EncodingJobInfo.TranscodeReasons' \
               Enum.TryParse), plus the AudioCodecNotSupported apps/ferrofin-server planner.rs \
               plan adds to audio it re-encodes (TryStreamCopy's AddTranscodeReason); NowViewingItem is \
               report_now_viewing_item's (ReportNowViewingItem) and UserPrimaryImageTag is \
               stamped by upsert_session (GetSessionInfo, GetImageCacheTag); \
               LastPlaybackCheckIn is DateTime.MinValue until update_now_playing stamps it \
               (SessionInfo.LastPlaybackCheckIn, UpdateNowPlayingItem) and \
               NowPlayingQueueFullItems is never sent (v12.4's SessionInfoDto has none); the \
               pool loses a session where upstream's does (report_session_ended, close_if_needed; \
               ReportSessionEnded, CloseIfNeededAsync) and also to evict_idle_sessions",
        date: "2026-11-11",
        divergences: SESSIONS_DIVERGENCES,
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Sessions/Playing",
        upstream_file: "Jellyfin.Api/Controllers/PlaystateController.cs",
        upstream_method: "ReportPlaybackStart",
        ferrofin: "handlers/playstate.rs report_playback_start (validate_play_method: \
               ValidatePlayMethod over AppState::hls get_transcoding_job, \
               TranscodeManager.GetTranscodingJob; current_session_id: RequestHelpers.GetSessionId) \
               → ferrofin-core session_manager.rs on_playback_start (SessionManager.OnPlaybackStart: \
               update_live_stream_session_mapping, update_now_playing for UpdateNowPlayingItem \
               with the report's own Item as sent, else the NowPlayingItem kept or named on the \
               library row it read (NowPlayingItem::of), the ClearTranscodingInfo \
               of a report that does not transcode, record_playback_start per session user on \
               progress_item for GetProgressItem only for a library item, the log line and the \
               PlaybackStart event's MediaInfo (NowPlayingItem::media_info: \
               reported_media_info or playback_media_info) named from info.Item, the now-playing \
               item, and its Item (playback_library_item) the library item) → \
               playback_activity.rs (PlaybackStartLogger, from that MediaInfo and Item: the \
               entry's ItemId and the theme-media check from Item, reading nothing)",
        date: "2026-11-11",
        divergences: PLAYBACK_START_DIVERGENCES,
        refs: &["user-data"],
    },
    Verified {
        method: "post",
        path: "/PlayingItems/{itemId}",
        upstream_file: "Jellyfin.Api/Controllers/PlaystateController.cs",
        upstream_method: "OnPlaybackStart",
        ferrofin: "handlers/playstate.rs on_playback_start (LegacyStartQuery: the query members \
               with canSeek false and PlayMethod Transcode when absent) → the POST \
               /Sessions/Playing path from validate_play_method on",
        date: "2026-11-11",
        divergences: PLAYBACK_START_DIVERGENCES,
        refs: &["user-data"],
    },
    Verified {
        method: "post",
        path: "/Sessions/Playing/Progress",
        upstream_file: "Jellyfin.Api/Controllers/PlaystateController.cs",
        upstream_method: "ReportPlaybackProgress",
        ferrofin: "handlers/playstate.rs report_playback_progress (validate_play_method: \
               ValidatePlayMethod; AppState::hls ping_transcoding_job, the keep-alive upstream's \
               TranscodeManager.OnPlaybackProgress runs off the PlaybackProgress event, here \
               before the report reaches the session manager) → \
               ferrofin-core session_manager.rs on_playback_progress \
               (SessionManager.OnPlaybackProgress: kept_now_playing_item, else the row the \
               session holds for that ItemId (library_row, FullNowPlayingItem) or the one read \
               for GetNowPlayingItem, the report's own Item as sent in its place \
               (NowPlayingItem::of), update_now_playing with the check-in only when not automated, \
               the ClearTranscodingInfo of a report that does not transcode, update_play_progress \
               per session user for OnPlaybackProgress(user, item, info) only for a library item, \
               update_live_stream_session_mapping, the PlaybackProgress event carrying the \
               now-playing item's NowPlayingItem::media_info for MediaInfo = info.Item and \
               playback_library_item for Item)",
        date: "2026-11-11",
        divergences: PLAYBACK_PROGRESS_DIVERGENCES,
        refs: &["user-data"],
    },
    Verified {
        method: "post",
        path: "/PlayingItems/{itemId}/Progress",
        upstream_file: "Jellyfin.Api/Controllers/PlaystateController.cs",
        upstream_method: "OnPlaybackProgress",
        ferrofin: "handlers/playstate.rs on_playback_progress (LegacyProgressQuery: the query \
               members with isPaused and isMuted false, PlayMethod Transcode and RepeatMode \
               RepeatNone when absent) → the POST /Sessions/Playing/Progress path from \
               validate_play_method on",
        date: "2026-11-11",
        divergences: PLAYBACK_PROGRESS_DIVERGENCES,
        refs: &["user-data"],
    },
    Verified {
        method: "post",
        path: "/Sessions/Playing/Stopped",
        upstream_file: "Jellyfin.Api/Controllers/PlaystateController.cs",
        upstream_method: "ReportPlaybackStopped",
        ferrofin: "handlers/playstate.rs report_playback_stopped (finish_playback_stop, on a \
               task the request awaits and a hang-up does not cut off; kill_session_transcodes: \
               KillTranscodingJobs, only for a PlaySessionId that is not blank, awaited with its \
               partial-file deletion) → ferrofin-core \
               session_manager.rs on_playback_stopped \
               (SessionManager.OnPlaybackStopped: the negative-position refusal after releasing the \
               live stream, the reported NowPlayingQueue and PlaylistItemId, RemoveNowPlayingItem's \
               PlayState reset and ClearTranscodingInfo, save_stopped_play_state per session user \
               unless the playback failed, close_live_stream_if_needed, the log line and the \
               PlaybackStopped event's MediaInfo named from info.Item (PlaybackSession::stopped_item): \
               the stop's own Item as sent (reported_media_info), else the now-playing item when \
               the stop names it (info.Item = current), else the row the stop read \
               (playback_media_info), none once \
               the library no longer has it; its Item (playback_library_item) the library item; \
               the idle stop carries a reported Item the session shows, as CheckForIdlePlayback's \
               Item = session.NowPlayingItem) → playback_activity.rs (PlaybackStopLogger, from \
               that MediaInfo and Item, reading nothing)",
        date: "2026-11-11",
        divergences: PLAYBACK_STOP_DIVERGENCES,
        refs: &["user-data"],
    },
    Verified {
        method: "delete",
        path: "/PlayingItems/{itemId}",
        upstream_file: "Jellyfin.Api/Controllers/PlaystateController.cs",
        upstream_method: "OnPlaybackStopped",
        ferrofin: "handlers/playstate.rs on_playback_stopped (LegacyStopQuery: mediaSourceId, \
               nextMediaType, positionTicks, liveStreamId, playSessionId) → the POST \
               /Sessions/Playing/Stopped path from finish_playback_stop on",
        date: "2026-11-11",
        divergences: PLAYBACK_STOP_DIVERGENCES,
        refs: &["user-data"],
    },
    Verified {
        method: "post",
        path: "/Sessions/Viewing",
        upstream_file: "Jellyfin.Api/Controllers/SessionController.cs",
        upstream_method: "ReportViewing",
        ferrofin: "handlers/session.rs report_viewing (a missing or blank itemId a 400 before \
               anything else, as [Required] model validation; sessionId absent or blank the \
               caller's own, RequestHelpers.GetSessionId; controlling_session_id → \
               assert_can_control: \
               GetSession's 404 and AssertCanControl's 403) → ferrofin-core session_manager.rs \
               report_now_viewing_item (SessionManager.ReportNowViewingItem: the library item, \
               with no user check, built by get_item_info as GetItemInfo(item, null), an unknown \
               item GetItemInfo's ArgumentNullException 400), shown by GET /Sessions",
        date: "2026-11-02",
        divergences: &[
            "An itemId that is not a GUID answers 400; upstream's new Guid(itemId) throws a \
         FormatException, a 500. Jellyfin bug not ported.",
            "The 400 (a missing, malformed or unknown itemId), 403 and 404 bodies are Ferrofin's \
         error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/UserImage",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "GetUserImage",
        ferrofin: "handlers/images.rs get_user_image (UserImageQuery; ImageQuery::tagged, \
               no sizes and quality 90) → serve_image_file (GetImageInternal, \
               GetImageResult) → ferrofin-drawing processor.rs process_image \
               (ImageProcessor.ProcessImage) → image_encoder.rs encode_image \
               (SkiaEncoder.EncodeImage) → limits.rs ImageMemory (SkiaEncoder.Decode, GetBitmap). \
               Only userId, tag and format are bound, so a size or a quality has no effect \
               and a malformed one is no 400; every request reaches the processor. userId \
               falls back to the caller, a nil result is a 400, no user or no profile image \
               a 404, and no credentials are needed, as upstream. The hidden \
               get_user_image_legacy and get_user_image_by_index_legacy \
               (LegacyUserImageQuery; GetUserImageLegacy, GetUserImageByIndexLegacy) take \
               userId from the route and bind every member those actions declare, then pass \
               on only tag and format: measured on 12.4, a malformed size, quality, blur, \
               percentPlayed or unplayedCount is a 400 there, a valid one has no effect, and \
               imageIndex is a query member only on the form with no index in its route",
        date: "2026-10-31",
        divergences: &[
            "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &["images"],
    },
    Verified {
        method: "get",
        path: "/Items/{itemId}/Images/{imageType}",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "GetItemImage",
        ferrofin: concat!(
            "handlers/images.rs get_item_image (ImageQuery: every member the action \
             declares is bound, so a malformed number is a 400; imageIndex from the query \
             through the binder's int rules, 0 when absent) → ",
            item_image_tail!()
        ),
        date: "2026-11-03",
        divergences: ITEM_IMAGE_DIVERGENCES,
        refs: &["images"],
    },
    Verified {
        method: "head",
        path: "/Items/{itemId}/Images/{imageType}",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "GetItemImage",
        ferrofin: "HeadItemImage is GetItemImage's own action: handlers/images.rs \
               get_item_image, the GET row's path, with ServeFile sending the headers alone \
               (measured on 12.4, Range included)",
        date: "2026-11-03",
        divergences: ITEM_IMAGE_DIVERGENCES,
        refs: &["images"],
    },
    Verified {
        method: "get",
        path: "/Items/{itemId}/Images/{imageType}/{imageIndex}",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "GetItemImageByIndex",
        ferrofin: concat!(
            "handlers/images.rs get_item_image_by_index (ImageQuery: every member the action \
             declares is bound, so a malformed number is a 400; the index is the route's, \
             and the query's imageIndex, which the action does not declare, is not read) → ",
            item_image_tail!()
        ),
        date: "2026-11-03",
        divergences: ITEM_IMAGE_DIVERGENCES,
        refs: &["images"],
    },
    Verified {
        method: "head",
        path: "/Items/{itemId}/Images/{imageType}/{imageIndex}",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "GetItemImageByIndex",
        ferrofin: "HeadItemImageByIndex is GetItemImageByIndex's own action: \
               handlers/images.rs get_item_image_by_index, the GET row's path, with ServeFile \
               sending the headers alone",
        date: "2026-11-03",
        divergences: ITEM_IMAGE_DIVERGENCES,
        refs: &["images"],
    },
    Verified {
        method: "get",
        path: "/Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "GetItemImage2",
        ferrofin: concat!(
            "handlers/images.rs get_item_image_parametrized (the route's imageIndex, maxWidth, \
             maxHeight and unplayedCount bind with the query binder's int rules, trimmed and \
             with the 0x, # and &h hex forms, percentPlayed with its double rules \
             (DoubleModelBinder: NumberStyles.Float | AllowThousands in the invariant culture, \
             so 1,000 binds and inf is a 400; no hex), format as the ImageFormat enum and tag \
             as a required string (RequiredRouteString: a blank one is the 400), so a malformed \
             member is a 400; tag, format and both sizes are passed on as given, a 0 or \
             negative size and a tag of 0 included; PositionalImageQuery binds only the query \
             members the action declares, so a malformed maxWidth, percentPlayed or imageIndex \
             in the query is not read; measured on 12.4) → ",
            item_image_tail!()
        ),
        date: "2026-11-15",
        divergences: ITEM_IMAGE_DIVERGENCES,
        refs: &["images"],
    },
    Verified {
        method: "head",
        path: "/Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "GetItemImage2",
        ferrofin: "HeadItemImage2 is GetItemImage2's own action: handlers/images.rs \
               get_item_image_parametrized, the GET row's path, with ServeFile sending the \
               headers alone (measured on 12.4)",
        date: "2026-11-11",
        divergences: ITEM_IMAGE_DIVERGENCES,
        refs: &["images"],
    },
    Verified {
        method: "delete",
        path: "/Items/{itemId}/Images/{imageType}",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "DeleteItemImage",
        ferrofin: "handlers/images.rs delete_item_image (RequireAdmin, as RequiresElevation; \
               ImageIndexQuery: imageIndex is the action's only query member, so a malformed \
               one is the binder's 400 before the item is read, a blank one is 0, and no other \
               query parameter is read, measured on 12.4; require_item, GetItemById(itemId, \
               User.GetUserId()): a missing item, or one a signed-in user cannot see, is a 404, \
               and an API key's caller is not checked) → ferrofin-providers \
               provider_manager.rs delete_image (BaseItem.DeleteImageAsync: a missing slot or a \
               negative index is nothing to do, with no save; otherwise a local row's file is \
               deleted and the item saved with ItemUpdateType.ImageUpdate) → ferrofin-core \
               item_persistence_service.rs delete_item_image (GetImages(type) \
               .ElementAtOrDefault(index), the row's path expanded as ExpandVirtualPath)",
        date: "2026-11-11",
        divergences: &[
            "A file that cannot be deleted still removes the image: the row goes first and the \
             file error is dropped, a 204, where DeleteImageAsync deletes the file first and a \
             failure is a 500 that keeps the image (measured on 12.4). Fix path: gl-1663.",
            "A Chapter image's file is not deleted: chapter images are not image rows, so the \
             delete finds nothing and answers 204, where GetImageInfo returns the chapter's \
             image and DeleteImageAsync deletes its file. Fix path: gl-1663.",
            "A file in the shared album-covers directory is kept, as an album and its tracks \
             name the same file there (docs/MUSIC_ARTWORK.md); DeleteImageAsync deletes any \
             local file, a track's extracted cover that the album's other tracks share \
             (AudioImageProvider.GetAudioImagePath) included, which leaves them naming a \
             missing file. Whether tracks keep the album's cover is gl-692.",
            "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["images"],
    },
    Verified {
        method: "post",
        path: "/UserImage",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "PostUserImage",
        ferrofin: "handlers/images.rs post_user_image (UserIdQuery, userId alone) → \
               handlers/items.rs resolve_user, handlers/users.rs assert_can_update_user, \
               handlers/image_upload.rs (try_get_image_extension_from_content_type → \
               ferrofin-model mime_types.rs to_extension, decode_base64) → ferrofin-core \
               user_manager.rs save_profile_image. The hidden post_user_image_legacy and \
               post_user_image_by_index_legacy (PostUserImageLegacy, \
               PostUserImageByIndexLegacy) take userId from the route and bind no query \
               member, measured on 12.4",
        date: "2026-10-31",
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
            "A body past Kestrel's 30,000,000-byte default (MAX_UPLOAD_BODY_BYTES, the same \
         cap) is a 413; upstream's ExceptionMiddleware maps no BadHttpRequestException, so \
         Kestrel's \"request body too large\" reaches the client as a plain-text 500. \
         Jellyfin bug not ported.",
            "The 400 (a nil effective user, a Content-Type that is not an image type, a body \
         that is not base64, a username outside the users directory, or a value the query \
         binder cannot convert), 403 (another user's id without the administrator role, or \
         a user without EnableUserPreferenceAccess) and 404 (no such user) bodies are \
         Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "delete",
        path: "/UserImage",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "DeleteUserImage",
        ferrofin: "handlers/images.rs delete_user_image (UserIdQuery, userId alone) → \
               handlers/items.rs resolve_user, handlers/users.rs assert_can_update_user → \
               ferrofin-core user_manager.rs clear_profile_image. The hidden \
               delete_user_image_legacy (DeleteUserImageLegacy) and \
               delete_user_image_by_index_legacy (DeleteUserImageByIndexLegacy) take userId \
               from the route; the first binds its one query member, int? index, and drops \
               it, so ?index=x is a 400 there, and the second binds none, measured on 12.4",
        date: "2026-10-31",
        divergences: &[
            "The 400 (a nil effective user, or a value the query binder cannot convert), 403 \
         (another user's id without the administrator role, or a user without \
         EnableUserPreferenceAccess) and 404 (no such user) bodies are Ferrofin's error \
         shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Audio/{itemId}/Lyrics",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "GetLyrics",
        ferrofin: "handlers/lyrics.rs get_lyrics (require_item: GetItemById's 400 for a nil \
               id) → ferrofin-core lyric_manager.rs get_lyrics (the item's Lyric streams, \
               LrcLyricParser then TxtLyricParser)",
        date: "2026-11-11",
        divergences: &[
            "A Lyric stream whose file cannot be read (removed outside the server before the next \
         refresh, or unreadable) is skipped and the next stream read; upstream's \
         File.ReadAllTextAsync throws on the first such stream, a plain-text 404 when the \
         file or its folder is gone and a 500 otherwise, so a later stream's lyric is never \
         served. Jellyfin bug not ported.",
            "The 400 (nil item id) and 404 (no such item, one the caller may not see, or no \
         lyrics) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Audio/{itemId}/Lyrics",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "UploadLyrics",
        ferrofin: "handlers/lyrics.rs upload_lyrics (require_item with GetItemById's 400 for \
               a nil id, an empty body by its size hint, upload_format, then the Bytes read \
               under upload_body_limit; the save and queue_high_priority_refresh on \
               handlers.rs run_detached's task, so they finish for a client that hangs up \
               once the body is read, as SaveLyricAsync takes no token) → ferrofin-core \
               lyric_manager.rs save_lyric \
               (decode_stream_text, parse_by_format on the format as given, save_targets \
               with save_extension, try_save_to_files)",
        date: "2026-11-11",
        divergences: &[
            "An upload with no Content-Length (a chunked body) is read and saved; upstream \
         tests Request.ContentLength and answers every chunked upload 400 \"No lyrics \
         uploaded\". Jellyfin bug not ported.",
            "A body past Kestrel's 30,000,000-byte default (MAX_UPLOAD_BODY_BYTES, the same \
         cap) is a 413 once the item, the empty-body and the extension checks pass, where \
         upstream reads it too; upstream's ExceptionMiddleware maps no \
         BadHttpRequestException, so Kestrel's \"request body too large\" reaches the \
         client as a plain-text 500. Jellyfin bug not ported.",
            "The 400 (a nil item id, an empty body, a file name without an extension, a format \
         no parser reads, a missing or blank fileName) and 404 (no such item, or one the \
         caller may not see) bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "delete",
        path: "/Audio/{itemId}/Lyrics",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "DeleteLyrics",
        ferrofin: "handlers/lyrics.rs delete_lyrics (require_item: GetItemById's 400 for a \
               nil id; the delete and run_item_refresh on handlers.rs run_detached's task, so \
               they finish for a client that hangs up, as DeleteLyricsAsync's \
               RefreshMetadata(CancellationToken.None) does) → ferrofin-core lyric_manager.rs \
               delete_lyrics",
        date: "2026-11-11",
        divergences: &[
            "The 400 (nil item id) and 404 (no such item, or one the caller may not see) bodies \
         are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Audio/{itemId}/RemoteSearch/Lyrics",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "SearchRemoteLyrics",
        ferrofin: "handlers/lyrics.rs search_remote_lyrics (require_item: GetItemById's 400 \
               for a nil id) → ferrofin-core lyric_manager.rs search_lyrics \
               (search_request_for, search_request, search_provider)",
        date: "2026-11-11",
        divergences: &[
            "The 400 (nil item id) and 404 (no such item, or one the caller may not see) bodies \
         are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Audio/{itemId}/RemoteSearch/Lyrics/{lyricId}",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "DownloadRemoteLyrics",
        ferrofin: "handlers/lyrics.rs download_remote_lyrics (RequiredRouteString lyricId: \
               ConvertEmptyStringToNull, then [Required]; require_item: GetItemById's 400 for \
               a nil id; the download and queue_high_priority_refresh on handlers.rs \
               run_detached's task, so they finish for a client that hangs up, as \
               DownloadLyricsAsync gets CancellationToken.None) → ferrofin-core \
               lyric_manager.rs download_lyrics (fetch_remote, save_targets, \
               try_save_to_files)",
        date: "2026-11-11",
        divergences: &[
            "The 400 (nil item id) and 404 (no such item, one the caller may not see, or no \
         lyric for the id) bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Providers/Lyrics/{lyricId}",
        upstream_file: "Jellyfin.Api/Controllers/LyricsController.cs",
        upstream_method: "GetRemoteLyrics",
        ferrofin: "handlers/lyrics.rs get_remote_lyrics (RequiredRouteString lyricId: \
               ConvertEmptyStringToNull, then [Required]) → ferrofin-core lyric_manager.rs \
               get_remote_lyrics (fetch_remote, parse_by_format)",
        date: "2026-10-30",
        divergences: &[
            "The 404 (no provider owns the id, or it has no such lyric) body is Ferrofin's \
         error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Localization/Countries",
        upstream_file: "Jellyfin.Api/Controllers/LocalizationController.cs",
        upstream_method: "GetCountries",
        ferrofin: "handlers/localization.rs get_countries → ferrofin-core localization_manager.rs \
               get_countries (COUNTRIES, data/countries.json byte-equal to UPSTREAM_TAG's)",
        date: "2026-10-28",
        divergences: &[],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Branding/Splashscreen",
        upstream_file: "Jellyfin.Api/Controllers/ImageController.cs",
        upstream_method: "GetSplashscreen",
        ferrofin: "handlers/branding.rs get_splashscreen (only tag and format are bound; \
               !SplashscreenEnabled is a 404 unless the caller is an administrator; \
               resolve_splashscreen_path: SplashscreenLocation when that file exists, else \
               {DataPath}/splashscreen.png, else a 404; is_file is File.Exists, false for a \
               directory, stat'd through tokio::fs) → handlers/images.rs \
               ImageQuery::splashscreen (no sizes, quality 90, and none of the DLNA \
               headers, as GetSplashscreen passes GetImageResult none) → serve_image_file \
               (GetImageResult: ProcessImage on every request, so a format the client's \
               output formats do not list is re-encoded) → ferrofin-drawing processor.rs \
               process_image. Measured on 12.4 with a BMP and a PNG splashscreen, as an \
               administrator: plain and If-Modified-Since",
        date: "2026-11-05",
        divergences: &[
            "Age, Last-Modified on a 304 and the If-Modified-Since check count from the \
         splashscreen file's modification time. Upstream's ItemImageInfo carries no \
         DateModified, so a splashscreen served as stored has an Age counted from \
         0001-01-01 and answers any If-Modified-Since with a 304, and a client that cached \
         one never sees its replacement. Jellyfin bug not ported.",
            "The 404 body is Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
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
        date: "2026-10-28",
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
            "A body past Kestrel's 30,000,000-byte default (MAX_UPLOAD_BODY_BYTES, the same \
         cap) is a 413; upstream's ExceptionMiddleware maps no BadHttpRequestException, so \
         Kestrel's \"request body too large\" reaches the client as a plain-text 500. \
         Jellyfin bug not ported.",
            "The 400 (a Content-Type that is not an image type) body is Ferrofin's error shape, \
         not upstream's JSON string. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Users/AuthenticateByName",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "AuthenticateUserByName",
        ferrofin: "handlers/users.rs authenticate_by_name, authenticate, login_failure, \
               authentication_result → ferrofin-core session_manager.rs authenticate_internal \
               (AuthenticateNewSessionInternal), admit_session → user_manager.rs \
               authenticate_user (UserManager.AuthenticateUser), authentication_providers_for \
               (GetAuthenticationProviders(User)), authenticate_with_providers \
               (AuthenticateLocalUser) and apply_external_administrator; a 401 or 403 \
               answers as ExceptionMiddleware does \
               outside Development, the same on a live 12.4 for each refusal. Compared: user \
               lookup and lock, provider order and selection by the saved id \
               (AuthenticationManager::id, GetType().FullName), the warning naming the user \
               and the saved id when no enabled provider has it (InvalidAuthProvider then \
               refuses the sign-in), the provider's canonical username, the new-user \
               policy (saved with update_policy, which announces UserUpdated: the push, \
               then the device check, as UpdatePolicyAsync does), the saved provider id, \
               the disabled, remote and schedule checks, \
               the failure counter and lockout, the device and session limits, token \
               rotation (device_manager.rs query_devices for DeviceManager.GetDevices: the \
               user's rows for the device id in Id order, then Logout(Device) of each) and \
               session activity, and that upstream has no login rate limit. \
               The UserDto and SessionInfoDto \
               builders are shared with other routes and not compared here. User, compared on \
               2026-11-15 (gl-1719), is user_manager.rs get_user_dto of the user \
               authenticate_internal signed in, as upstream's is GetUserDto of the user it \
               holds; it reads the database where upstream's reads memory, so it is built \
               before the token is minted and a read error fails the sign-in with nothing \
               minted, as a store error elsewhere in the sign-in fails it upstream",
        date: "2026-11-05",
        divergences: &[
            "User reads the database (the user's permissions, preferences, access schedules \
         and profile image) where upstream's GetUserDto reads the user it holds in memory, \
         so a read error there answers 500 with nothing minted; upstream cannot fail at that \
         step. Ferrofin keeps users in the database, not in memory (no domain-object \
         hierarchy, CLAUDE.md).",
            "The 400 (no client identity, an empty username) and 500 bodies are Ferrofin's error \
         shape, not upstream's. Fix path: gl-666.",
            "MaxActiveSessions counts the user's sessions in the pool, as upstream \
         (SessionManager.cs Sessions.Count), but the pool ends a session with no WebSocket that \
         plays nothing once it is idle past session_idle_hours (the GET /Sessions row), so \
         stale HTTP-only sessions stop counting then; upstream counts them until the server \
         restarts. Owner decision (gl-1267, 2026-11-05).",
            "Ferrofin's login backoff (owner decisions on gl-1099; upstream has no rate limit): \
         past login_backoff_attempts failures an account's and a client address's next \
         attempts wait, from 1 s doubling to login_backoff_max_delay_secs. An address that \
         would wait longer is answered as a wrong password without its password being \
         checked; an account only waits. The backoff never disables an account.",
            "An authentication provider reports the user's administrator flag in its result \
         (ProviderAuthenticationResult.administrator) and the user manager applies it \
         (UserManager::apply_external_administrator), never demoting the last administrator: \
         that sign-in succeeds, the user stays one and a warning is logged. Upstream's LDAP \
         plugin writes the flag itself through UpdateUserAsync, which has no such guard. No \
         built-in provider reports a flag. \
         Owner decision (2026-11-01). Like UpdateUserAsync, applying it announces nothing (no \
         UserUpdated push, no device check), so an administrator a provider demotes keeps \
         the devices their policy refuses signed in until a later policy save or rename, as \
         upstream does (gl-1191 asks the owner whether to keep that).",
            "A provider may say a password it refuses never counts toward lockout \
         (AuthenticationManager::counts_failed_passwords, read for the provider the account \
         names, enabled or not): such a refusal answers the usual 401 and leaves \
         InvalidLoginAttemptCount and the enabled flag alone, where upstream's AuthenticateUser \
         counts every refused password. Only the Single Sign-On extension's provider says so, \
         since it refuses every password of the accounts it owns; the built-in providers and \
         LDAP-Auth count as upstream does, and the login backoff still paces the caller. \
         Owner decision (D20 of the OIDC sign-in milestone, 2026-11-20).",
            "Every failed login writes the dashboard's AuthenticationFailed entry, where upstream \
         publishes AuthenticationRequestEventArgs only for a wrong password: an exception from \
         AuthenticateUser (an unknown user, a disabled account) or a device or session-limit \
         refusal skips it, so those attempts never reach the Alerts feed. Jellyfin bug not \
         ported.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Users/AuthenticateWithQuickConnect",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "AuthenticateWithQuickConnect",
        ferrofin: "handlers/users.rs authenticate_with_quick_connect, login_failure, \
               quick_connect_user, authentication_result → ferrofin-core \
               quick_connect_manager.rs get_authorized_request (the result authorize_request \
               stored); the fallback to the stored User compared on 2026-11-15 (gl-1719) \
               against QuickConnectManager.GetAuthorizedRequest",
        date: "2026-11-01",
        divergences: &[
            "The User member is built at the exchange, from the user as it is then, where \
         upstream returns the UserDto it snapshotted when the code was authorized, up to \
         10 minutes stale; a user deleted meanwhile, or whose DTO cannot be read, gets \
         that snapshot, as upstream answers, though its LastActivityDate can predate the \
         session's activity stamp upstream's carries (built before it). The session, \
         token and ServerId are the stored ones on both. Jellyfin bug not ported.",
            "The 404 (unknown or expired secret) body is Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Users/ForgotPassword",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "ForgotPassword",
        ferrofin: "handlers/users.rs forgot_password (ForgotPasswordDto, extract.rs \
               RequiredString and body_parameter, ip_access.rs is_local_connection) → \
               ferrofin-core user_manager.rs start_forgot_password_process \
               (UserManager.StartForgotPasswordProcess), password_reset_provider_for \
               (GetPasswordResetProvider(User?)) → password_reset.rs \
               DefaultPasswordResetProvider (issue_reset_pin). Compared: the [Required] \
               required string EnteredUsername (missing, null, empty and whitespace are 400s \
               before the action, with Jellyfin 12.4's messages, measured), the body's \
               forgotPasswordRequest entry when it fails to bind (measured), isLocal as \
               IsLocal() || IsInLocalNetwork(ip), the warning for a request from outside the \
               local network, the user lookup, the provider chosen by the saved \
               PasswordResetProviderId among the enabled ones with the built-in fallback, the \
               PIN file written only for an existing user on the local network, its name \
               (the entered name's invariant-uppercase MD5), the 30-minute expiry, the PIN's \
               format, and the PinCode answer every caller gets",
        date: "2026-11-02",
        divergences: &[
            "After a local request for an existing user, upstream saves the user unchanged \
         (UpdateUserAsync), which deletes and re-adds their access schedules, so each one's \
         Id in UserPolicy.AccessSchedules changes. Ferrofin saves nothing. Jellyfin bug not \
         ported.",
            "The 500 body (a PIN file that cannot be written) is Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Users/ForgotPassword/Pin",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "ForgotPasswordPin",
        ferrofin: "handlers/users.rs forgot_password_pin (ForgotPasswordPinDto, extract.rs \
               RequiredString and body_parameter) → ferrofin-core user_manager.rs \
               redeem_password_reset_pin (UserManager.RedeemPasswordResetPin), redeem_with → \
               password_reset.rs \
               DefaultPasswordResetProvider (redeem_reset_pins), change_password, \
               log_password_changed. Compared: the [Required] required string Pin (missing, \
               null, empty and whitespace are 400s before the action, with Jellyfin 12.4's \
               messages, measured), the body's forgotPasswordPinRequest entry when it fails \
               to bind (measured), every provider asked in order whether enabled or not, \
               the first success answered, an expired record deleted, the PIN compared \
               ordinally with its dashes removed, the password set to the PIN as entered, \
               the UserPasswordChanged activity entry, the record removed once the password \
               is saved, a record whose user is gone or that reads as null a 404, and no \
               match a 404",
        date: "2026-11-02",
        divergences: &[
            "A PIN that no built-in record matches is offered to each extension's \
         password-reset provider in turn. Upstream's DefaultPasswordResetProvider is \
         registered before every plugin's and throws ResourceNotFoundException when nothing \
         matches, so RedeemPasswordResetPin never reaches a plugin's provider. Jellyfin bug \
         not ported.",
            "The built-in provider reads every record before it changes any password, so an \
         unreadable record fails the request with nothing reset. Upstream resets each match as \
         it walks the directory, so the matches before the failing record (in directory \
         order) are applied and their files deleted. Jellyfin bug not ported.",
            "The 404 (no provider redeemed the PIN) and 500 bodies are Ferrofin's error shape, \
         not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/QuickConnect/Initiate",
        upstream_file: "Jellyfin.Api/Controllers/QuickConnectController.cs",
        upstream_method: "InitiateQuickConnect",
        ferrofin: "handlers/quick_connect.rs initiate → ferrofin-core quick_connect_manager.rs \
               try_connect (make_room, draw_free_code, generate_code, generate_secret)",
        date: "2026-11-11",
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
            "The 400 (a missing DeviceId, Device, Client or Version) and 401 (Quick Connect \
         disabled) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
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
               store_listing_provider) → schedules_direct.rs SchedulesDirect::validate \
               (ListingsManager.SaveListingProvider, XmlTvListingsProvider.Validate, \
               SchedulesDirect.Validate)",
        date: "2026-11-11",
        divergences: &[
            "An XMLTV provider with no Path, or a null one, is a 400; upstream's Validate \
         dereferences the null path and answers a NullReferenceException 500 (an empty or \
         blank Path is the FileNotFoundException 404 on both). Jellyfin bug not ported.",
            "Schedules Direct's /token body is serialized, so a username with a quote or a \
         backslash stays valid JSON; upstream concatenates it into the body raw. Jellyfin \
         bug not ported.",
            "SchedulesDirect.GetTokenInternal logs the session token at info; Ferrofin never \
         logs the password, its hash or the token. Jellyfin bug not ported.",
            "A null EnabledTuners or ChannelMappings is saved and answered as the empty list, \
         and a null category list (answered without the member, as upstream does) reads \
         back as its default keywords: the values upstream reads back from livetv.xml after \
         a restart. Until then upstream keeps the null, so GET /System/Configuration/livetv \
         omits the member (and so does upstream's own answer, for the two lists), mapping \
         the guide dereferences a null ChannelMappings (a NullReferenceException), and with \
         EnableAllTuners off the tuner check calls Contains on a null EnabledTuners (an \
         ArgumentNullException). Jellyfin bug not ported.",
            "A null element in a list (\"EnabledTuners\":[null], \"ChannelMappings\":[null] or \
         a category list) is a 400; upstream binds and stores it (200, measured on Jellyfin \
         12.4 for the first two). Fix path: gl-1677.",
            "The 400 (an XMLTV provider with no Path, a Schedules Direct save missing its \
         Username, Password or ListingsId, or a query value the binder cannot convert), 401 \
         (Schedules Direct refuses the login) and 404 (an unknown or missing Type, or an \
         XMLTV file that is not there) bodies are Ferrofin's error shape, not upstream's. \
         Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/LiveTv/ListingProviders/Lineups",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetLineups",
        ferrofin: "handlers/live_tv.rs get_lineups → ferrofin-livetv manager.rs get_lineups \
               (ListingsBackend::of, provider_channels: xmltv_cache.rs XmlTvCache::get_xml; \
               with no copy cached, a local file or directory that is not there is a 404 \
               through fetch.rs missing_file_is_not_found, an empty Path a 400) → \
               schedules_direct.rs \
               SchedulesDirect::get_lineups (ListingsManager.GetLineups, \
               XmlTvListingsProvider.GetLineups and GetXml, ExceptionMiddleware.GetStatusCode, \
               SchedulesDirect.GetHeadends)",
        date: "2026-11-17",
        divergences: &[
            "An XMLTV guide declaring an encoding name the WHATWG labels know but .NET's \
         encoding table does not (iso8859-9, iso88599, x-cp1254, …) is decoded by the \
         WHATWG encoding; upstream's XmlReader refuses the name, so the guide is unreadable \
         and Lineups answers 500. Fix path: gl-939.",
            "An XMLTV guide using an undeclared namespace prefix is read, where upstream's \
         XmlReader refuses it (500); a <tv> that held programmes but no channel, followed by \
         something not well-formed, is unreadable here (500) where upstream's GetChannels \
         stops at </tv> and answers []; and prefixed channel elements are matched by local \
         name. Fix path: gl-942.",
            "A blank id is a 404 whatever the type; upstream calls \
         GetProvider(type).GetLineups(null, …), which is a 404 for an unknown type but \
         dereferences the null provider (500) for xmltv and SchedulesDirect. Jellyfin bug \
         not ported.",
            "The country and postal code are percent-encoded into Schedules Direct's \
         headends query; upstream concatenates them raw, so a value holding `&` or `#` \
         changes the query. Jellyfin bug not ported.",
            "An XMLTV provider with no Path (only an adopted livetv.xml stores one) is read \
         from a stale cached copy; upstream's GetXml dereferences the null Path before it \
         falls back to one (a NullReferenceException 500). With no copy cached both answer \
         500. Jellyfin bug not ported.",
            "An XMLTV guide at a .gz or .gzip URL that its server sends with \
         Content-Encoding: gzip, which the HTTP client decodes as upstream's \
         AutomaticDecompression does, is read, as is any such URL whose body does not start \
         with gzip's magic number; upstream's UnzipIfNeededAndCopy runs GZipStream over \
         those bytes, logs the failure and then fails the download as empty, so the guide \
         is not read, or stays stale where an older copy is cached. Jellyfin bug not ported.",
            "The 400 (an empty Path) and 404 (a blank id, no such provider, or a \
         local XMLTV file or directory that is not there) bodies are Ferrofin's error shape, \
         not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/LiveTv/TunerHosts",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "AddTunerHost",
        ferrofin: "handlers/live_tv.rs add_tuner_host → ferrofin-livetv manager.rs \
               save_tuner_host (tuner_host::find_host, guide_repository::existing_config_id, \
               TunerHost::validate, guide_repository::store_tuner_host), then \
               queue_guide_refresh (TunerHostManager.SaveTunerHost, M3UTunerHost.Validate, \
               HdHomerunHost.Validate)",
        date: "2026-10-28",
        divergences: &[
            "An M3U tuner without a Url, or with a null one, is a 400; upstream's \
         M3uParser.GetListingsStream dereferences the null Url and answers a \
         NullReferenceException 500. Jellyfin bug not ported.",
            "The 404 (an unknown or missing tuner Type, or an M3U playlist file that is not \
         there) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/LiveTv/Channels",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetLiveTvChannels",
        ferrofin: "handlers/live_tv.rs get_channels (items.rs resolve_user_opt, \
               program_dto_options, add_current_program) → ferrofin-livetv manager.rs \
               get_channels (user_restrictions; guide_repository.rs channel_rows: only the \
               lineup channels whose item is stored, the user's parental restrictions, and \
               isMovie/isSeries over the stored channel item's \
               columns and isKids/isNews/isSports over its Kids/News/Sports tags; \
               channel_user_data; projection.rs filter_channel_rows, sort_channel_rows, \
               remove_fields; channel_entity → DtoService get_base_item_dtos, which runs \
               AddChannelInfo) (RequestHelpers.GetUserId, LiveTvManager.GetInternalChannels, \
               BaseItemRepository.TranslateQuery, RemoveFields)",
        date: "2026-11-10",
        divergences: &[
            "type keeps the channels of that ChannelType, as the parameter documents; \
         upstream's GetInternalChannels never passes LiveTvChannelQuery.ChannelType on and \
         lists every channel. Jellyfin bug not ported.",
            "Every sortBy key but SortName, Name, IsFavoriteOrLiked and DateCreated compares \
         equal, so the appended SortName decides; upstream's OrderMapper sorts on them. Fix \
         path: gl-1354.",
            "isFavorite and isLiked with no user (an API key naming none) match no channel \
         when true and every channel when false, and enableFavoriteSorting sorts nothing; \
         upstream's TranslateQuery and OrderMapper dereference the null user \
         (filter.User!.Id) and fail the request. Jellyfin bug not ported.",
            "The 400 (a value the query binder cannot convert) and 403 (another user's id \
         without the administrator role) bodies are Ferrofin's error shape, not upstream's. \
         Fix path: gl-666.",
        ],
        refs: &["access-filtering", "user-data"],
    },
    Verified {
        method: "get",
        path: "/LiveTv/Recordings",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetRecordings",
        ferrofin: "handlers/live_tv.rs get_recordings (items.rs resolve_user_opt, \
               program_dto_options; ListedRecordingVisibility → recording_visible_given, \
               holding_libraries over ferrofin-model entities_media.rs libraries_holding, \
               libraries_allow) → ferrofin-livetv manager.rs \
               get_recordings_matching (dvr_repository.rs recording_rows, \
               recording_file_is_gone, visible_recording_rows, remove_fields, \
               recording_dtos, add_info_to_recording_dtos) (RequestHelpers.GetUserId, \
               LiveTvManager.GetRecordingsAsync, GetEmbyRecordingsAsync, \
               GetRecordingFoldersAsync, LibraryManager.GetCollectionFolders, RemoveFields)",
        date: "2026-11-05",
        divergences: &[
            "Recordings are the rows of Ferrofin's DVR table, not library items under the \
         recording folders, so the list differs wherever the two models do: an id is the \
         row's, not the library item's; a recording file with no row (one from an adopted \
         Jellyfin database, or a partial file) is not listed; a finished recording whose file \
         has gone from a folder that is still there leaves the list at once, where upstream \
         lists the library item until a scan removes it; channelId, status, seriesTimerId \
         and isNews filter the rows, where upstream ignores them; isMovie, isSeries, isKids \
         and isSports match the programme's flags, either way, where upstream matches the \
         Movie and Episode kinds and, for true only, the Sports, Kids, Children and Family \
         genres; isInProgress=false filters the rows, where upstream ignores it, and \
         isInProgress=true keeps limit and counts every capture in progress, where upstream \
         drops limit and counts the page; the order is the capture's creation time, newest \
         first, then the name; enableTotalRecordCount=false still counts every recording, \
         where upstream reports the page's length; the parental rating, blocked and allowed \
         tag and unrated rules see a Video with no rating or tags, where upstream applies \
         them to the library item, its library's tags included; and a recording no library \
         holds is listed, where upstream lists only under the recording folders the user may \
         see, and answers an empty page when there is none. Fix path: gl-1077.",
            "A request with no user (an API key naming none, or an administrator naming an \
         account that does not exist) lists every recording, as GET \
         /LiveTv/Recordings/{recordingId} answers the same request for each, while GET \
         /LiveTv/Recordings/Folders answers it an empty list; upstream's \
         GetEmbyRecordingsAsync answers an empty page for a null user. Deliberate: API keys \
         are exempt from the library rule gl-1091 made a hard boundary.",
            "The 400 (a value the query binder cannot convert) and 403 (another user's id \
         without the administrator role) bodies are Ferrofin's error shape, not upstream's. \
         Fix path: gl-666.",
        ],
        refs: &["access-filtering"],
    },
    Verified {
        method: "get",
        path: "/LiveTv/Programs",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetLiveTvPrograms",
        ferrofin: "handlers/live_tv.rs get_programs (ProgramsQuery::into_parts: parse_csv_uuids, \
               parse_pipe_strings, parse_csv_enums_lenient, query_parse.rs parse_order_by, \
               program_dto_options; items.rs resolve_user_opt) → query_programs_inner (the \
               StartDate default order, the librarySeriesId block) → ferrofin-livetv \
               manager.rs get_programs (stored_series_timer for seriesTimerId, \
               user_restrictions, program_rows_query (the stored programme item joined by \
               id) over push_program_filters, push_program_order with \
               push_program_sort_key and push_program_paging, the PROGRAM_COUNT total, \
               projection.rs remove_fields, program_dtos → program_entity and dto_service.rs \
               get_base_item_dtos, apply_program_info, add_recording_info → \
               scheduled_timers_for over dvr_repository.rs scheduled_timers_for_programs, \
               timer_program_ids, apply_recording_info, then series_timers_by_name over \
               dvr_repository.rs series_timer_series_ids for a programme its timer gave no \
               series timer) (RequestHelpers.GetUserId, RequestHelpers.GetOrderBy, \
               DtoExtensions.AddAdditionalDtoOptions, LiveTvManager.GetPrograms, \
               LibraryManager.QueryItems, BaseItemRepository.TranslateQuery and ApplyOrder, \
               OrderMapper, RemoveFields, DtoService.GetBaseItemDtos, AddInfoToProgramDto, \
               AddRecordingInfo, GetTimersInternal, DefaultLiveTvService.GetTimersAsync, \
               GetSeriesTimersInternal)",
        date: "2026-11-15",
        divergences: &[
            "With no user (an API key), sortBy PlayCount, IsFavoriteOrLiked, IsPlayed or \
         IsUnplayed sorts by SortName, as /Items does; upstream's OrderMapper reads the \
         missing user's id (query.User!.Id), which throws. Jellyfin bug not ported.",
            "The listing's fields (Name, Overview, OfficialRating, Genres, the flags, \
         PremiereDate, Audio and an HD listing's Width and Height) are read from the guide \
         table, so a change saved to a programme's item (POST /Items/{itemId}) is not shown \
         here. CommunityRating, which no listing sets, is never shown. Upstream lists the \
         stored item, the change included, until a guide refresh that updates the programme \
         assigns the listing's values over it. A PremiereDate an earlier listing stored is \
         no longer shown once a later one makes the airing a first-run episode, nor is a \
         1280x720 size once the listing is not HD; upstream's item keeps both. Fix path: \
         gl-1690.",
            "A malformed id in channelIds or genreIds is a 400; upstream's \
         CommaDelimitedCollectionModelBinder drops it. Fix path: gl-884.",
            "The 400 (a value the query binder cannot convert) and 403 (another user's id \
         without the administrator role) bodies are Ferrofin's error shape, not upstream's. \
         Fix path: gl-666.",
        ],
        refs: &[
            "access-filtering",
            "images",
            "paging-sort",
            "guide-programme-items",
        ],
    },
    Verified {
        method: "post",
        path: "/LiveTv/Programs",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetPrograms",
        ferrofin: "handlers/live_tv.rs post_programs (GetProgramsDto::into_parts, \
               get_order_by, program_dto_options; items.rs resolve_user_opt for a non-nil \
               body UserId only) → query_programs_inner (the StartDate default order, the \
               LibrarySeriesId block) → ferrofin-livetv manager.rs get_programs \
               (stored_series_timer for SeriesTimerId, user_restrictions, program_rows_query \
               (the stored programme item joined by id) over push_program_filters, \
               push_program_order with push_program_sort_key and push_program_paging, the \
               PROGRAM_COUNT total, projection.rs remove_fields, program_dtos → \
               program_entity and dto_service.rs get_base_item_dtos, apply_program_info, \
               add_recording_info → scheduled_timers_for over dvr_repository.rs \
               scheduled_timers_for_programs, timer_program_ids, apply_recording_info, then \
               series_timers_by_name over dvr_repository.rs series_timer_series_ids for a \
               programme its timer gave no series timer) (GetProgramsDto, \
               RequestHelpers.GetOrderBy, DtoExtensions.AddAdditionalDtoOptions, \
               LiveTvManager.GetPrograms, LibraryManager.QueryItems, \
               BaseItemRepository.TranslateQuery and ApplyOrder, OrderMapper, RemoveFields, \
               DtoService.GetBaseItemDtos, AddInfoToProgramDto, AddRecordingInfo, \
               GetTimersInternal, DefaultLiveTvService.GetTimersAsync, GetSeriesTimersInternal)",
        date: "2026-11-15",
        divergences: &[
            "A body UserId naming another user, sent without the administrator role, is a 403 \
         (the RequestHelpers.GetUserId rule GET applies); upstream's GetPrograms reads \
         body.UserId through GetUserById with no such check, so any Live TV user can list \
         the guide as another user, with that user's user data and parental restrictions. \
         Jellyfin bug not ported.",
            "An offset-less MinStartDate, MaxStartDate, MinEndDate or MaxEndDate (or a bare \
         date) is read as UTC; upstream's GetDateTime leaves it Unspecified and EF's \
         DateTimeKindValueConverter reads it as server-local time, as the GET form here \
         does. Fix path: gl-1033.",
            "With no body UserId, SortBy PlayCount, IsFavoriteOrLiked, IsPlayed or \
         IsUnplayed sorts by SortName, as /Items does; upstream's OrderMapper reads the \
         missing user's id (query.User!.Id), which throws. Jellyfin bug not ported.",
            "The listing's fields (Name, Overview, OfficialRating, Genres, the flags, \
         PremiereDate, Audio and an HD listing's Width and Height) are read from the guide \
         table, so a change saved to a programme's item (POST /Items/{itemId}) is not shown \
         here. CommunityRating, which no listing sets, is never shown. Upstream lists the \
         stored item, the change included, until a guide refresh that updates the programme \
         assigns the listing's values over it. A PremiereDate an earlier listing stored is \
         no longer shown once a later one makes the airing a first-run episode, nor is a \
         1280x720 size once the listing is not HD; upstream's item keeps both. Fix path: \
         gl-1690.",
            "The 400 (a body that does not bind) body is Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[
            "access-filtering",
            "images",
            "paging-sort",
            "guide-programme-items",
        ],
    },
    Verified {
        method: "delete",
        path: "/LiveTv/TunerHosts",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "DeleteTunerHost",
        ferrofin: "handlers/live_tv.rs delete_tuner_host → ferrofin-livetv manager.rs \
               delete_tuner_host, then queue_guide_refresh → refresh_guide's clean_database \
               prune (TunerHostManager.DeleteTunerHost, GuideManager.CleanDatabase)",
        date: "2026-10-28",
        divergences: &[],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/LiveTv/ChannelMappingOptions",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetChannelMappingOptions",
        ferrofin: "handlers/live_tv.rs get_channel_mapping_options (RequireAdmin for \
               RequiresElevation) → ferrofin-livetv manager.rs get_channel_mapping_options \
               (listing_provider_by_id, ListingsBackend::of, channels_for_listings_provider's \
               live read of every tuner through its host, matcher_channel, provider_channels) \
               → mapping.rs tuner_channel_mapping, epg_channel_for_tuner_channel, \
               EpgChannelData (ListingsManager.GetChannelMappingOptions, \
               GetChannelsForListingsProvider, BaseTunerHost.GetChannels, \
               GetTunerChannelMapping, GetEpgChannelFromTunerChannel, GetMappedChannel, \
               EpgChannelData, XmlTvListingsProvider.GetChannels and GetXml, \
               ExceptionMiddleware.GetStatusCode, SchedulesDirect.GetChannels). \
               A tuner channel's Id is the ChannelInfo.Id its host mints, hdhr_{GuideNumber} \
               for an HDHomeRun since gl-1751. provider_channels reads the guide through \
               xmltv_cache.rs XmlTvCache::get_xml: with no copy cached, a local file or \
               directory that is not there is a 404 (fetch.rs missing_file_is_not_found, \
               FileNotFoundException and DirectoryNotFoundException), an empty Path a 400 \
               (ArgumentException)",
        date: "2026-11-17",
        divergences: &[
            "An absent providerId, or one that names no listings provider, is a 404; \
         upstream's .First(...) throws InvalidOperationException, a 500. Jellyfin bug not \
         ported.",
            "The tuners of each type are read in id order, not configuration order, so their \
         channels are listed in that order, and a channel two tuners of one type both list is \
         kept for the one read first. Fix path: gl-762.",
            "An M3U entry with neither tvg-id nor channel-id is matched to the guide by its \
         display name (or a CUID attribute) as its TunerChannelId; upstream's is null, so its \
         match goes on to the number. Fix path: gl-751.",
            "An XMLTV guide declaring an encoding name the WHATWG labels know but .NET's \
         encoding table does not is decoded, where upstream's XmlReader refuses it and the \
         options answer 500. Fix path: gl-939.",
            "An XMLTV guide using an undeclared namespace prefix is read, where upstream's \
         XmlReader refuses it (500); a <tv> that held programmes but no channel, followed by \
         something not well-formed, is unreadable here (500) where upstream's GetChannels \
         stops at </tv> and lists no provider channels; and prefixed channel elements are \
         matched by local name. Fix path: gl-942.",
            "A ChannelMappings saved as null reads as the empty list; upstream keeps the null \
         until a restart and, once a tuner channel is listed, dereferences it (a \
         NullReferenceException 500). Jellyfin bug not ported.",
            "An XMLTV provider with no Path (only an adopted livetv.xml stores one) is read \
         from a stale cached copy; upstream's GetXml dereferences the null Path before it \
         falls back to one (a NullReferenceException 500). With no copy cached both answer \
         500. Jellyfin bug not ported.",
            "An XMLTV guide at a .gz or .gzip URL that its server sends with \
         Content-Encoding: gzip, which the HTTP client decodes as upstream's \
         AutomaticDecompression does, is read, as is any such URL whose body does not start \
         with gzip's magic number; upstream's UnzipIfNeededAndCopy runs GZipStream over \
         those bytes, logs the failure and then fails the download as empty, so the guide \
         is not read, or stays stale where an older copy is cached. Jellyfin bug not ported.",
            "The 400 (an empty Path), 404 (no such provider, an unknown provider \
         Type, or a local XMLTV file or directory that is not there) and 500 (a guide that \
         cannot be read) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/LiveTv/ChannelMappings",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "SetChannelMapping",
        ferrofin: "handlers/live_tv.rs set_channel_mapping (RequireAdmin for \
               RequiresElevation; SetChannelMappingDto::validation, each [Required] member \
               left out, null or blank a 400) → ferrofin-livetv manager.rs set_channel_mapping \
               (listing_provider_by_id, the channelMappingExists toggle, \
               store_listing_provider as SaveConfiguration, without the XMLTV cache \
               invalidation SaveListingProvider adds, channels_for_listings_provider, \
               provider_channels, mapping.rs tuner_channel_mapping), then queue_guide_refresh \
               (ListingsManager.SetChannelMapping, \
               CancelIfRunningAndQueue<RefreshGuideScheduledTask>). The tuner list and the \
               match ladder are the ChannelMappingOptions row's, so a mapping keyed \
               hdhr_{GuideNumber} applies since gl-1751, and so is the guide read: a local \
               XMLTV file or directory that is not there, with no copy cached, is a 404 after \
               the pair is stored and before the refresh is queued, as upstream's \
               FileNotFoundException is",
        date: "2026-11-17",
        divergences: &[
            "A ProviderId that names no listings provider is a 404 before anything is stored, \
         and a TunerChannelId no tuner channel has is a 404 after the pair is stored; \
         upstream's .First(...) throws InvalidOperationException (a 500) at the same two \
         points, and both queue the guide refresh before the second. Jellyfin bug not \
         ported.",
            "The tuners of each type are read in id order, not configuration order, which \
         decides the tuner a channel two tuners of one type share is kept for, and so whether \
         a provider enabled for only one of them lists it. Fix path: gl-762.",
            "An M3U entry with neither tvg-id nor channel-id is matched to the guide by its \
         display name (or a CUID attribute) as its TunerChannelId; upstream's is null, so its \
         match goes on to the number. Fix path: gl-751.",
            "The guide's channels are read, after the pair is stored, with the \
         ChannelMappingOptions row's XMLTV differences: encoding names .NET does not know \
         (gl-939) and undeclared namespace prefixes (gl-942) are read where upstream answers \
         500, a <tv> with programmes but no channel followed by something not well-formed is \
         a 500 here where upstream reads no provider channels and goes on (gl-942), prefixed \
         channel elements are matched by local name (gl-942), and an XMLTV provider with no \
         Path (only an adopted livetv.xml stores one) is read from a stale cached copy where \
         upstream's GetXml dereferences the null (a NullReferenceException 500; Jellyfin bug \
         not ported). Fix path: gl-939, gl-942.",
            "A ChannelMappings saved as null reads as the empty list; upstream keeps the null \
         until a restart, and its .Any(...) throws ArgumentNullException before anything is \
         stored (a 400). Jellyfin bug not ported.",
            "An XMLTV guide at a .gz or .gzip URL that its server sends with \
         Content-Encoding: gzip, which the HTTP client decodes as upstream's \
         AutomaticDecompression does, is read, as is any such URL whose body does not start \
         with gzip's magic number; upstream's UnzipIfNeededAndCopy runs GZipStream over \
         those bytes, logs the failure and then fails the download as empty, so the guide \
         is not read, or stays stale where an older copy is cached. Jellyfin bug not ported.",
            "The 400 (a body that does not bind, or an empty Path), 404 and 500 (a \
         guide that cannot be read) bodies are Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/{itemId}/ContentType",
        upstream_file: "Jellyfin.Api/Controllers/ItemUpdateController.cs",
        upstream_method: "UpdateItemContentType",
        ferrofin: "handlers/item_update.rs update_item_content_type (GetItemById's 400 for a \
               nil id) → ferrofin-core library_manager.rs containing_folder_path \
               (BaseItem/Video/Playlist and the by-name ContainingFolderPath overrides) → \
               ferrofin-model content_type_overrides.rs with_content_type_override",
        date: "2026-11-11",
        divergences: &[
            "The 400 (nil item id) and 404 (no such item, or one the caller may not see) bodies \
         are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/System/Configuration/{key}",
        upstream_file: "Jellyfin.Api/Controllers/ConfigurationController.cs",
        upstream_method: "GetNamedConfiguration",
        ferrofin: "handlers/config.rs get_named_configuration (RequiredRouteString key: \
               ConvertEmptyStringToNull, then [Required]) → ferrofin-core \
               configuration_manager.rs get_named_configuration and get_encoding_options \
               (read_named, through read_named_kept for branding and encoding; \
               BaseConfigurationManager.GetConfiguration and LoadConfiguration, the seven \
               ConfigurationStore registrations) → encoding_response for encoding, the Live \
               TV manager's tuner and provider rows for livetv, blank_for_non_administrator \
               for livetv and network",
        date: "2026-10-23",
        divergences: &[
            "The stored document is not cached from the first load until a save: branding and \
         encoding are kept until their file's stamp changes, and the other stored documents \
         are reread on every call. So a hand-edited file applies without a restart, and a \
         document that does not parse is logged at warn once per change of the file where \
         upstream logs it at error once.",
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
            "For a caller who is not an administrator, every string, list or object member of \
         livetv and network that handlers/config.rs does not list as open reads empty: today \
         a tuner's Url and UserAgent, a listing provider's \
         Username, Password, ListingsId, ZipCode, Path, UserAgent and ChannelMappings, livetv's \
         RecordingPostProcessor and RecordingPostProcessorArguments, and network's \
         CertificatePath and CertificatePassword. Upstream's plain [Authorize] serves them to \
         every signed-in user (owner decision 2026-10-23, widened by gl-1093). encoding stays \
         whole: the player reads it.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/System/Configuration/{key}",
        upstream_file: "Jellyfin.Api/Controllers/ConfigurationController.cs",
        upstream_method: "UpdateNamedConfiguration",
        ferrofin: "handlers/config.rs update_named_configuration (RequiredRouteString key: \
               ConvertEmptyStringToNull, then [Required]; normalize_named_configuration: \
               GetConfigurationType and Deserialize) → save_named_configuration, on \
               handlers.rs run_detached's task so it finishes for a client that hangs up, in \
               the request's span and culture \
               (BaseConfigurationManager.SaveConfiguration: the validating store, the write, \
               the live network policy (ferrofin-networking NetworkManager::update_settings: \
               UpdateSettings, warning of each LAN or exclusion entry InitializeLan ignores \
               and logging an error for each override InitializeOverrides cannot parse, the \
               loop stopping at the first without exactly one '=' as upstream's does, then \
               PrintNetworkInformation(config, false): its seven Information lines), the \
               listeners (ferrofin-server listen.rs \
               restart_listener: the port restart of OnConfigurationUpdated, with \
               ferrofin-core configuration_manager.rs set_ports_changed and \
               revoke_port_authorization, its clear and save of IsPortAuthorized), the committed \
               checks for ApplicationHost.OnConfigurationUpdated) → for livetv \
               LiveTvManager::create_recording_folders (ferrofin-livetv recording_folders.rs: \
               RecordingsManager.OnNamedConfigurationUpdated, which the Live TV manager's tuner \
               and provider saves run too, as upstream's save of livetv does)",
        date: "2026-11-15",
        divergences: &[
            "database is refused with a 400: Ferrofin has no other database provider (owner \
         decision 2026-10-23).",
            "livetv's TunerHosts and ListingProviders are the Live TV manager's rows, kept in \
         the database, so lists in a posted document are stored with it but neither add nor \
         change a tuner or a provider.",
            "A null list in livetv (TunerHosts, ListingProviders, MediaLocationsCreated, or a \
         provider's EnabledTuners or ChannelMappings) is saved as the empty list, the value \
         upstream reads back from livetv.xml after a restart; until then upstream keeps the \
         null and its GET omits the member.",
            "The save answers once CreateRecordingFolders has finished, where upstream's async \
         void handler runs alongside it. It still cannot fail the save.",
            "The first CreateRecordingFolders on a data directory forgets the \
         MediaLocationsCreated entries that are not recording folders instead of removing \
         them from their libraries: adoption carries that list over but not the recording \
         paths, so upstream's rule would delete the library holding the recordings.",
            "A network save whose BaseUrl changes the base URL this lifetime serves \
         (FERROFIN_BASE_URL outranks it) marks a restart pending, and the redirects and \
         advertised URLs keep the running value until then. Upstream's \
         OnConfigurationUpdated flags no restart for BaseUrl, while its redirect middleware \
         and GetLocalApiUrl read the new value at once and its mount keeps the old one \
         (owner decision on gl-555).",
            "A network save marks a restart pending, as OnConfigurationUpdated does, when the \
         ports it resolves to differ from the ports this lifetime listens on. Resolving \
         differs from upstream's raw comparison in three ways: --port/FERROFIN_PORT and \
         FERROFIN_HTTPS_PORT outrank InternalHttpPort and InternalHttpsPort (owner decision \
         on gl-554); a saved port at its default (8096, 8920) leaves config.toml's, as an empty \
         BaseUrl does (gl-554's fix, reading the owner's precedence); and equal saved ports \
         are reset to the defaults first, as ApplicationHost.Init does, where upstream flags a \
         restart that changes nothing for a save of two equal ports. Upstream's guard on a \
         running port of 0 (the ports not read yet) needs no counterpart: the listener is \
         registered once the ports are resolved. Its guard on IsPortAuthorized is not \
         ported: the restart is flagged whenever the ports differ. That matches upstream's \
         once-per-lifetime flag, since IsPortAuthorized is set at every boot and a raised \
         restart flag stays raised, except after a client saved IsPortAuthorized false with \
         no restart pending, where upstream announces no restart for a port change that \
         needs one. Jellyfin bug not ported.",
            "When a network save changes a port served, IsPortAuthorized is cleared and saved, \
         as OnConfigurationUpdated does, but on a task of its own (a configuration listener \
         may not save), so it may still read true for a moment after the save answers; \
         upstream's NamedConfigurationUpdated handler saves it before. While the ports \
         differ, a later server configuration save (POST /System/Configuration, which has \
         no row) stores it false in the same write, where upstream saves the posted value \
         and its ConfigurationUpdated handler clears it in a second save.",
            "An unparsable PublishedServerUriBySubnet entry is logged with its URL cut to \
         scheme, host and port (docs/conventions/LOGGING.md rule 5), where upstream logs the \
         entry whole.",
            "The 400 and 500 bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/LiveTv/Recordings/Folders",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetRecordingFolders",
        ferrofin: "handlers/live_tv.rs get_recording_folders (resolve_user_opt, then \
               LiveTvManager.GetRecordingFoldersAsync: FindByPath, IsVisibleStandalone and \
               GetCollectionFolders over VirtualFolderManager::library_folders) → \
               ferrofin-livetv recording_folders.rs recording_folders \
               (RecordingsManager.GetRecordingFolders)",
        date: "2026-10-30",
        divergences: &[
            "No channel's folder is added (GetChannelsInternalAsync with IsRecordingsFolder): \
         Ferrofin has no channel plugins, by design.",
            "A request with no user (an API key without userId, or a userId naming no account) \
         answers an empty list. Upstream reads user.Id and fails with a 500; Jellyfin bug not \
         ported.",
            "A location resolves to the libraries holding it from their configured locations, \
         not through FindByPath's stored Folder row, compared ignoring case. So a new library \
         is answered before its first scan created that row, and so is a location no scan has \
         stored under a larger library.",
            "SortName compares ordinally rather than through OrderBy's culture comparer.",
            "The 403 (another user's id without elevation) body is Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/LiveTv/Timers/{timerId}",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetTimer",
        ferrofin: "handlers/live_tv.rs get_timer (RequiredRouteString: a blank timerId is a 400) → \
               find_timer (LiveTvManager.GetTimer: GetTimers with TimerQuery.Id, then the first \
               whose Id equals timerId ignoring case; none is 200 null, unchecked as upstream) → \
               ferrofin-livetv manager.rs get_timers_matching (get_timers, \
               DefaultLiveTvService.GetTimersAsync: every timer but a Completed one; \
               ferrofin-traits live_tv.rs filter_timers) → build_timer_dto for each \
               (LiveTvDtoService.GetTimerInfoDto: program_row_for_timer and program_dtos with \
               DtoOptions::default and no user for ProgramInfo, GetBaseItemDto(program, new \
               DtoOptions()) with AddInfoToProgramDto, then TimerId and Status unless Cancelled \
               or Error, SeriesTimerId, and for a series timer's timer fill_images, \
               FillImages(BaseItemDto, …) by the timer's name and the programme's \
               ExternalSeriesId over first_with_image, item_images and image_tag; \
               guide_repository.rs channel_row and manager.rs channel_image_tags for ChannelName and \
               ChannelPrimaryImageTag, none without the channel; RunTimeTicks from the window; \
               ProgramId as GetInternalProgramId(info.ProgramId) through derive_program_id over \
               the stored listing id, as stored without one)",
        date: "2026-11-15",
        divergences: &[
            "ProgramInfo is the programme DTO GET /LiveTv/Programs projects from the guide's \
         own table, so it shows the listing's fields, not a change saved to the programme's \
         item, nor a PremiereDate or 1280x720 size an earlier listing stored. Fix path: \
         gl-1690.",
            "A series timer's timer whose name is blank runs no lookup by name, so its \
         ProgramInfo takes no Thumb or Backdrop from a library series, and without a listings \
         series id no Primary from a guide programme; upstream's FillImages then filters on \
         nothing and takes the library's first series with a Thumb and the guide's first \
         programme with a Primary. Fix path: gl-1790, an owner decision.",
            "The 400 body is Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["images", "guide-programme-items"],
    },
    Verified {
        method: "delete",
        path: "/LiveTv/Timers/{timerId}",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "CancelTimer",
        ferrofin: "handlers/live_tv.rs cancel_timer (RequiredRouteString) → find_timer \
               (LiveTvManager.CancelTimer's GetTimer: none is a 404) → ferrofin-livetv \
               manager.rs cancel_timer → cancel_timer_internal (DefaultLiveTvService.\
               CancelTimerAsync → CancelTimerInternal(id, false, true): Cancelled and manual, \
               deleted without a SeriesTimerId, else kept; disarm_timer and cancel_recording for \
               RecordingsManager.CancelRecording) → notify_timer_event TimerCancelled only when \
               the status was not already Cancelled (statusChanging), to the sessions of the \
               users holding EnableLiveTvAccess (RecordingNotifier.SendMessage: ferrofin-core \
               session_manager.rs send_message_to_sessions_with_permission)",
        date: "2026-11-14",
        divergences: &[
            "TimerCancelled carries the timer's own Id, as Ferrofin's TimerCreated does. \
         Upstream's default service relays its external id for both \
         (OnEmbyTvTimerCancelled, OnEmbyTvTimerCreated), so jellyfin-web's onTimerCancelled \
         clears only a card its own TimerCreated push marked, never one rendered with the \
         programme's TimerId (cardBuilder's data-timerid). Jellyfin bug not ported.",
            "The 404 body is Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/LiveTv/Timers",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetTimers",
        ferrofin: "handlers/live_tv.rs get_timers (TimersQuery: channelId, seriesTimerId, \
               isActive, isScheduled) → ferrofin-livetv manager.rs get_timers_matching \
               (LiveTvManager.GetTimers: get_timers, DefaultLiveTvService.GetTimersAsync's every \
               timer but a Completed one; ferrofin-traits live_tv.rs filter_timers: isActive as \
               InProgress, isScheduled as New, channelId and seriesTimerId compared as GUIDs, \
               ordered by StartDate) → build_timer_dto for each \
               (LiveTvDtoService.GetTimerInfoDto: program_row_for_timer and program_dtos with \
               DtoOptions::default and no user for ProgramInfo, GetBaseItemDto(program, new \
               DtoOptions()) with AddInfoToProgramDto, then TimerId and Status unless Cancelled \
               or Error, SeriesTimerId, and for a series timer's timer fill_images, \
               FillImages(BaseItemDto, …) by the timer's name and the programme's \
               ExternalSeriesId over first_with_image, item_images and image_tag; \
               guide_repository.rs channel_row and manager.rs channel_image_tags for ChannelName and \
               ChannelPrimaryImageTag, none without the channel; RunTimeTicks from the window; \
               ProgramId as GetInternalProgramId(info.ProgramId) through derive_program_id over \
               the stored listing id, as stored without one)",
        date: "2026-11-15",
        divergences: &[
            "A channelId or seriesTimerId that is no GUID matches no timer; upstream's new Guid \
         throws a FormatException, a 500. Jellyfin bug not ported.",
            "A seriesTimerId naming the id an empty series timer id hashes to (the new-timer \
         defaults' Id) matches no timer; upstream hashes each timer's empty SeriesTimerId to \
         that id (GetInternalSeriesTimerId), so it lists every timer of no series timer. \
         Jellyfin bug not ported.",
            "ProgramInfo is the programme DTO GET /LiveTv/Programs projects from the guide's \
         own table, so it shows the listing's fields, not a change saved to the programme's \
         item, nor a PremiereDate or 1280x720 size an earlier listing stored. Fix path: \
         gl-1690.",
            "A series timer's timer whose name is blank runs no lookup by name, so its \
         ProgramInfo takes no Thumb or Backdrop from a library series, and without a listings \
         series id no Primary from a guide programme; upstream's FillImages then filters on \
         nothing and takes the library's first series with a Thumb and the guide's first \
         programme with a Primary. Fix path: gl-1790, an owner decision.",
            "The 400 body (a value the query binder cannot convert) is Ferrofin's error shape, \
         not upstream's. Fix path: gl-666.",
        ],
        refs: &["images", "guide-programme-items"],
    },
    Verified {
        method: "get",
        path: "/LiveTv/Timers/Defaults",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetDefaultTimer",
        ferrofin: "handlers/live_tv.rs get_default_timer (TimerDefaultsQuery: \
               empty_as_none_uuid, so an empty or blank programId is none; a nil one is \
               GetItemById's 400) → ferrofin-livetv manager.rs get_new_timer_defaults \
               (LiveTvManager.GetNewTimerDefaults and GetNewTimerDefaultsInternal: \
               ferrofin-traits live_tv.rs new_timer_defaults for \
               DefaultLiveTvService.GetNewTimerDefaultsAsync, with live_tv_options' padding; for \
               a programme, program_row and its name, overview, channel, window, RecordNewOnly \
               and SkipEpisodesInLibrary from IsRepeat, ProgramId and ExternalProgramId; then \
               GetSeriesTimerInfoDto's FillImages(dto, program.Name, null) through fill_images: \
               first_with_image, item_images and image_tag)",
        date: "2026-11-14",
        divergences: &[
            "A programId that does not parse is the binder's 400; upstream's new Guid throws a \
         FormatException, a 500. Jellyfin bug not ported.",
            "A programId naming no programme (an unknown id, or another kind of item) answers \
         the standing defaults; upstream dereferences the null programme or fails the cast, \
         a 500. Jellyfin bug not ported.",
            "Without a programId, or for a programme whose title is blank, the defaults carry \
         no artwork; upstream's FillImages with no name filters on nothing and gives them the \
         library's first series' Thumb, Backdrop and Primary. Fix path: gl-1790, an owner \
         decision.",
            "The 400 body is Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["images", "guide-programme-items"],
    },
    Verified {
        method: "get",
        path: "/LiveTv/SeriesTimers",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetSeriesTimers",
        ferrofin: "handlers/live_tv.rs get_series_timers (SeriesTimersQuery: sortBy, \
               sortOrder, Ascending when absent) → ferrofin-livetv manager.rs get_series_timers \
               (LiveTvManager.GetSeriesTimers: dvr_repository.rs series_timer_rows; \
               sort_series_timers, Priority then the name, else the name, names compared by \
               ferrofin-util numeric_collation.rs invariant_numeric_cmp, SortExtensions' \
               invariant-culture NumericOrdering comparer, a null name first; \
               build_series_timer_dto for each: the channel's current name from \
               guide_repository.rs channel_row, none without a channel, ProgramId as \
               GetInternalProgramId(info.ProgramId) through derive_program_id over the \
               stored listing id (as stored without one), and GetSeriesTimerInfoDto's \
               FillImages(dto, info.Name, info.SeriesId) through fill_images with the stored \
               SeriesId: first_with_image, item_images and image_tag)",
        date: "2026-11-15",
        divergences: &[
            "A series timer whose name is blank runs no lookup by name: it takes no Thumb or \
         Backdrop from a library series, and its Primary only from an airing of its listings \
         series. Upstream's FillImages then filters on nothing and takes the library's first \
         series' Thumb, Backdrop and Primary. Fix path: gl-1790, an owner decision.",
        ],
        refs: &["images"],
    },
    Verified {
        method: "get",
        path: "/LiveTv/SeriesTimers/{timerId}",
        upstream_file: "Jellyfin.Api/Controllers/LiveTvController.cs",
        upstream_method: "GetSeriesTimer",
        ferrofin: "handlers/live_tv.rs get_series_timer (RequiredRouteString: a blank timerId \
               is a 400; LiveTvManager.GetSeriesTimer: GetSeriesTimers with a default query, \
               then the first whose Id equals timerId ignoring case; none is a 404) → \
               ferrofin-livetv manager.rs get_series_timers (as GET /LiveTv/SeriesTimers: \
               series_timer_rows, build_series_timer_dto with ProgramId derived from the \
               stored listing id (as stored without one), and fill_images)",
        date: "2026-11-15",
        divergences: &[
            "A series timer whose name is blank runs no lookup by name: it takes no Thumb or \
         Backdrop from a library series, and its Primary only from an airing of its listings \
         series. Upstream's FillImages then filters on nothing and takes the library's first \
         series' Thumb, Backdrop and Primary. Fix path: gl-1790, an owner decision.",
            "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &["images"],
    },
    Verified {
        method: "post",
        path: "/Users/{userId}/Policy",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "UpdateUserPolicy",
        ferrofin: "handlers/users.rs update_user_policy (ferrofin-model UserPolicy's serde \
               default constructor_policy: new UserPolicy()'s values for a member the body \
               leaves out; policy_validation: UserPolicy's [Required(AllowEmptyStrings = false)] \
               provider ids; load_user: GetUserById's 400 for a nil id) → ferrofin-core \
               user_manager.rs update_policy_guarded (save_policy: UserManager.UpdatePolicyAsync \
               behind ferrofin-traits policy_update_refusal; announce_user_updated: its \
               UserUpdatedEventArgs and OnUserUpdated → ferrofin-core user_updated.rs, \
               UserUpdatedNotifier then DeviceAccessHost) → session_manager.rs \
               revoke_user_tokens",
        date: "2026-11-02",
        divergences: &[
            "The three guards (the last administrator, an administrator disabled, the last \
         enabled user) are read inside the transaction that saves, and a newly disabled \
         user's tokens are revoked once it committed. Upstream counts from its user cache \
         before saving and revokes first, so two concurrent saves could both pass a guard and \
         leave the server without an administrator or an enabled user. Jellyfin bug not \
         ported.",
            "Because the revocation waits for the guarded save, it comes after the save's \
         UserUpdated push and device check: a newly disabled user's open sessions hear \
         UserUpdated just before they are signed out, where upstream's are already gone when \
         UpdatePolicyAsync publishes. Their end state is the same.",
            "The 400 (nil user id), 403 (a guard refused) and 404 (unknown user) bodies are \
         Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Users",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "GetUsers",
        ferrofin: "handlers/users.rs get_users → filtered_user_dtos (the private Get with \
               filterByDevice and filterByNetwork off: IsHidden and IsDisabled through the \
               projected policy; OrderBy(u => u.Username), the current culture's comparer) → \
               ferrofin-core user_manager.rs get_user_dto (UserManager.GetUserDto: \
               build_user_policy, resolve_cast_receiver_id); strip_other_accounts",
        date: "2026-11-01",
        divergences: &[
            USER_FIELDS_STRIPPED,
            USER_LYRIC_MANAGEMENT,
            "The 400 (a value the query binder cannot convert) body is Ferrofin's error shape, \
         not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Users/Public",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "GetPublicUsers",
        ferrofin: "handlers/users.rs get_public_users (Get(false, false, false, false) until \
               the wizard is done, then Get(false, false, true, true): the device from the \
               authenticated claim only, and the live network policy's remote-access filter) → \
               filtered_user_dtos → ferrofin-core user_manager.rs get_user_dto; \
               strip_other_accounts, after the filters that read Policy",
        date: "2026-11-01",
        divergences: &[USER_FIELDS_STRIPPED, USER_LYRIC_MANAGEMENT],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Users/{userId}",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "GetUserById",
        ferrofin: "handlers/users.rs get_user_by_id (RequireAuthIgnoringSchedule: \
               IgnoreParentalControl; the route id binds through extract::DotnetGuid, \
               GuidConverter's trimmed Guid.Parse; load_user: GetUserById's 400 for a nil id) → \
               ferrofin-core user_manager.rs get_user_dto; strip_other_accounts",
        date: "2026-11-08",
        divergences: &[
            USER_FIELDS_STRIPPED,
            USER_LYRIC_MANAGEMENT,
            "The 400 (a nil or malformed user id) and 404 (unknown user) bodies are Ferrofin's \
         error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Users",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "UpdateUser",
        ferrofin: "handlers/users.rs update_user (also behind the hidden POST /Users/{userId}; \
               policy_validation for the nested Policy's [Required] provider ids; UserDto's \
               serde default for a missing Configuration; load_user: GetUserById's 400 for a \
               nil id, an API key's userId ?? User.GetUserId() included) → ferrofin-core \
               user_manager.rs rename_user (require_valid_username: ThrowIfInvalidUsername; \
               move_profile_image_on; announce_user_updated: RenameUser's \
               UserUpdatedEventArgs and OnUserUpdated → ferrofin-core user_updated.rs, \
               UserUpdatedNotifier then DeviceAccessHost), update_configuration (one writer \
               transaction, as one SaveChangesAsync; accepted_cast_receiver_id; announces \
               nothing)",
        date: "2026-11-02",
        divergences: &[
            "\"Configuration\": null skips the configuration save, where upstream dereferences \
         the null in UpdateConfigurationAsync and answers 500. Jellyfin bug not ported.",
            "A name ending in a newline (\"alice\\n\") is refused as invalid. Upstream's \
         ValidUsernameRegex ends in $, which .NET also matches before a final newline, so it \
         saves the name. Jellyfin bug not ported.",
            "A rename moves a profile image stored in the old name's directory to the new \
         name's directory, so its PrimaryImageTag changes, and a move that fails answers 500 \
         and rolls the rename back. \
         Upstream's RenameUser leaves the file where it is, and a later user given the old \
         name then overwrites the renamed user's image. Jellyfin bug not ported.",
            "The 400 (nil user id, invalid or taken name), 403 (update not allowed) and 404 \
         (unknown user) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Users/Configuration",
        upstream_file: "Jellyfin.Api/Controllers/UserController.cs",
        upstream_method: "UpdateUserConfiguration",
        ferrofin: "handlers/users.rs update_user_configuration (also behind the hidden \
               POST /Users/{userId}/Configuration; JsonBody for the [Required] body; \
               load_user: GetUserById's 400 for a nil id, an API key's \
               userId ?? User.GetUserId() included; \
               assert_can_update_user: RequestHelpers.AssertCanUpdateUser with \
               restrictUserPreferences) → ferrofin-core user_manager.rs update_configuration \
               (UserManager.UpdateConfigurationAsync: the Users columns and the OrderedViews, \
               GroupedFolders, MyMediaExcludes and LatestItemExcludes preferences in one writer \
               transaction; accepted_cast_receiver_id: a non-empty id naming a configured \
               receiver, else the stored one is kept); no UserUpdated push, as \
               UpdateConfigurationAsync raises no event",
        date: "2026-10-29",
        divergences: &[
            "The 400 (nil user id), 403 (update not allowed) and 404 (unknown user) bodies are \
         Ferrofin's error shape, not upstream's. Fix path: gl-666.",
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
        date: "2026-10-28",
        divergences: &[
            "isFavorite with no user (an API key without userId, or a userId naming no \
         account) is ignored and the totals are unfiltered (200); upstream's TranslateQuery \
         dereferences the null User for the favourites subquery and fails (500). Jellyfin bug \
         not ported.",
            "The 403 (a userId other than the caller's, from a caller who is not an \
         administrator) body is Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Auth/Keys",
        upstream_file: "Jellyfin.Api/Controllers/ApiKeyController.cs",
        upstream_method: "GetKeys",
        ferrofin: "handlers/api_key.rs get_keys (RequireAdmin, Policies.RequiresElevation) → \
               ferrofin-core api_key_manager.rs get_api_keys and to_authentication_info \
               (AuthenticationManager.GetApiKeys) → ferrofin-model security.rs \
               AuthenticationInfo, querying QueryResult::from_items",
        date: "2026-10-28",
        divergences: &[
            "The 401 (no or bad token) and 403 (not an administrator) bodies are Ferrofin's \
         error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Auth/Providers",
        upstream_file: "Jellyfin.Api/Controllers/SessionController.cs",
        upstream_method: "GetAuthProviders",
        ferrofin: "handlers/session.rs get_auth_providers (RequireAdmin, \
               Policies.RequiresElevation) → ferrofin-core user_manager.rs \
               get_authentication_providers (UserManager.GetAuthenticationProviders) over the \
               built-in default and the providers compiled-in extensions register \
               (set_extension_providers). Compared: the IsEnabled filter, the Default \
               provider first and the rest by Name in the current culture, and each pair's \
               Name and Id (the provider's GetType().FullName, AuthenticationManager::id)",
        date: "2026-11-02",
        divergences: &[
            "The 401 (no or bad token) and 403 (not an administrator) bodies are Ferrofin's \
         error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Auth/PasswordResetProviders",
        upstream_file: "Jellyfin.Api/Controllers/SessionController.cs",
        upstream_method: "GetPasswordResetProviders",
        ferrofin: "handlers/session.rs get_password_reset_providers (RequireAdmin, \
               Policies.RequiresElevation) → ferrofin-core user_manager.rs \
               get_password_reset_providers (UserManager.GetPasswordResetProviders) over the \
               built-in password_reset.rs DefaultPasswordResetProvider and the providers \
               compiled-in extensions register (set_extension_reset_providers). Compared: the \
               IsEnabled filter, the default provider first and the rest by Name in the \
               current culture, and each pair's Name and Id (GetType().FullName, \
               PasswordResetManager::id)",
        date: "2026-11-02",
        divergences: &[
            "The 401 (no or bad token) and 403 (not an administrator) bodies are Ferrofin's \
         error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Collections",
        upstream_file: "Jellyfin.Api/Controllers/CollectionController.cs",
        upstream_method: "CreateCollection",
        ferrofin: "handlers/collection.rs create_collection (parse_csv_uuids for the \
               CommaDelimitedCollectionModelBinder ids; parentId bound and unused, as \
               upstream) → ferrofin-core collection_manager.rs create_collection \
               (CollectionManager.CreateCollectionAsync: new_box_set_folder_and_id for the \
               [boxset] folder and AddChild's path-derived id, its Path stored through \
               VirtualPathExpander::reverse as AddChild's save stores it, \
               %AppDataPath%/collections/<name> [boxset] (BaseItemMapper.Map → \
               GetPathToSave → ReverseVirtualPath), the folder claimed and the row stored \
               under box_set_folders, which a Collections scan storing the box sets of \
               the folders it found (resolve_box_set_folders, BoxSetResolver) holds too, \
               AddToCollectionAsync's refusal of an id naming no item after the box set \
               is stored, then its UpdateToRepositoryAsync(MetadataEdit) through \
               save_membership → ProviderManager::update_to_repository, whose \
               ferrofin-providers box_set_xml.rs BoxSetXmlSaver writes the box set's \
               collection.xml (BoxSetXmlSaver, BaseXmlSaver.AddCommonNodesAsync and \
               AddLinkedChildren in local_xml.rs save_collection_xml, \
               ProviderManager.IsSaverEnabledForItem) and stamps DateLastSaved after it \
               (LibraryManager.UpdateItemsAsync)) and \
               ensure_collections_folder → collections_library.rs (EnsureLibraryFolder: \
               physical_folder for FindFolders, ensure_library for AddVirtualFolder with its \
               LibraryOptions, ensure_folder for the Folder row ValidateTopLibraryFolders \
               makes)",
        date: "2026-11-19",
        divergences: &[
            "A missing name makes a box set with an empty name, where upstream's \
         GetValidFilename dereferences the null and answers 500. Jellyfin bug not ported.",
            "A malformed id is a 400 before anything is written, and a whitespace-only entry \
         in ids is dropped. Upstream's new Guid(x) throws a FormatException on either once \
         the box set is stored, which answers 500 and leaves it behind. Jellyfin bug not \
         ported.",
            "A second box set of one name gets a folder of its own (<name>1 [boxset]). \
         Upstream reuses the folder, so the path-derived id is the first box set's and \
         AddChild overwrites it. Jellyfin bug not ported.",
            "The 400 bodies (a malformed id, an id naming no item) are Ferrofin's error \
         shape, not upstream's. Fix path: gl-666.",
            "The first collection's box-set refresh runs in a background task \
         (refresh_full_item) rather than the High-priority refresh queue, and the new \
         library's scan is queued once it has run, where upstream queues the scan when \
         AddVirtualFolder makes the library and the refresh through QueueRefresh. \
         Upstream's slower background scan almost always loses that race; Ferrofin's \
         would win it and give the box set a Default first refresh, which takes TMDb's \
         collection name over the typed one of a create with items. The scan this create \
         queues now always follows the refresh; a membership change or an unrelated scan \
         during that refresh can still race it, as upstream (gl-1622).",
            "collection.xml writes <Added> and <PremiereDate> in UTC and reads both <Taglines> \
         and <TagLines>; upstream writes the dates in the server's local time and reads \
         them back as UTC, which shifts them on every round trip on a server outside UTC, \
         and reads only the <TagLines> it never writes. On a UTC server both write the \
         same dates. Jellyfin bug not ported (owner decision 2026-11-14).",
            "A box set's refresh takes the <DisplayOrder> its collection.xml carries, and no \
         refresh resets the display order. Upstream starts the provider result from a new \
         BoxSet, whose DisplayOrder defaults to PremiereDate, so the file's value never \
         applies and every refresh a provider answered resets the box set's display order \
         to PremiereDate. Jellyfin bug not ported (owner decision 2026-11-14).",
            "The always-true BoxSet.IsSaveLocalMetadataEnabled holds only for a box set whose \
         folder is in the server's own directories (owner decision 2026-10-17); one in a media \
         library is written only when that library lists the Emby Xml saver or, with no saver \
         list, saves into its media folders, and otherwise an edit or member change never \
         rewrites a collection.xml already there, where upstream, in a library with no saver \
         list, writes a box set's file whatever SaveLocalMetadata says (owner decision \
         2026-11-19).",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Library/VirtualFolders",
        upstream_file: "Jellyfin.Api/Controllers/LibraryStructureController.cs",
        upstream_method: "GetVirtualFolders",
        ferrofin: "handlers/library_structure.rs get_virtual_folders → ferrofin-core \
               virtual_folder_manager.rs get_virtual_folders and list_libraries \
               (LibraryManager.GetVirtualFolders(true) and GetVirtualFolderInfo: \
               is_directory for Directory.EnumerateDirectories, resolve_locations and \
               resolve_shortcut for the Locations and their culture sort, \
               read_collection_type, load_options, stored_options and \
               adopt_jellyfin_options for CollectionFolder.GetLibraryOptions and \
               LoadLibraryOptions (its defaults, its error log, its ExpandVirtualPath), \
               scan_progress for RefreshProgress and RefreshStatus) → ferrofin-model \
               entities_media.rs VirtualFolderInfo, configuration.rs LibraryOptions, \
               TypeOptions, ImageOption, MediaPathInfo",
        date: "2026-11-14",
        divergences: &[
            "An unreadable shortcut is logged at error the first time a listing meets it and \
         at debug while it stays unreadable; upstream logs it at error on every listing. \
         The listing runs on every dashboard load, and docs/conventions/LOGGING.md rule 1 \
         keeps a level whose volume scales with the request rate out of error.",
            "A library directory without a CollectionFolder row gets one written by the \
         listing, so ItemId, LibraryOptions and RefreshStatus are always sent; upstream \
         leaves the folder members out until its in-memory root folder's children are \
         revalidated. Ferrofin has no in-memory tree, and the row keeps the library's id \
         resolvable and a scan clear of the foreign key (owner decision 2026-10-24).",
            "A failed write of that CollectionFolder row is logged at warn and the library \
         is listed with its projected ItemId; the next listing retries the write. No row is \
         written while a library-structure change is under way: the change records its own \
         library's locations, and a later listing writes any other row.",
            "An empty .mblink is dropped; upstream filters only nulls, so it lists an empty \
         location, which names no folder. Jellyfin bug not ported.",
            "An options.json that cannot be read or parsed serves the defaults, logged at \
         error the first time a read meets it and at debug while it stays so. Upstream \
         caches the options per library and logs a parse or permission failure at error \
         once per cache fill (a library-structure change clears it), and any other I/O \
         failure (an IOException) not at all. The listing reads the file on every \
         dashboard load, and docs/conventions/LOGGING.md rule 1 keeps a level whose \
         volume scales with the request rate out of error.",
            "An adopted options.xml that cannot be read or does not convert, with no \
         options.json beside it, serves the defaults and is read again on every listing, \
         logged at error the first time a read meets it and at debug while it stays so (at \
         error again after it has converted or been removed, or after an options.json that \
         took over is gone). Upstream logs a parse or permission failure at error once per fill of its \
         options cache, which a library-structure change clears, and any other I/O failure \
         not at all; the same LOGGING.md rule applies.",
            "The 401 and 403 bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "delete",
        path: "/Library/VirtualFolders",
        upstream_file: "Jellyfin.Api/Controllers/LibraryStructureController.cs",
        upstream_method: "RemoveVirtualFolder",
        ferrofin: "handlers/library_structure.rs remove_virtual_folder → finish_structure_change \
               (after_structure_change: LibraryMonitor::restart, then queue_library_scan when \
               refreshLibrary) → ferrofin-core virtual_folder_manager.rs remove_virtual_folder \
               (LibraryManager.RemoveVirtualFolder: child_path and require_folder for \
               GetChildPath and Directory.Exists, preserve_legacy_children, the CollectionFolder \
               row deleted, remove_dir_all, prune_orphan_collection_folders for \
               ValidateTopLibraryFolders' tail) → library_monitor.rs restart and run \
               (LibraryMonitor.Stop and Start, StartWatchingPath)",
        date: "2026-11-03",
        divergences: &[
            "Without refreshLibrary, upstream waits 1000 ms (Task.Delay) inside the request before \
         LibraryMonitor.Start, so its directory watchers miss the removal; Ferrofin answers at \
         once and the monitor restarts on its own task without the wait. The removal writes \
         only under the libraries root, which is not a library location, so no watcher sees it \
         either way.",
            "The monitor keeps watching while the folder is removed and restarts after; upstream \
         stops it first. For the reason above, no event of the removal reaches it.",
            "The library's CollectionFolder row is deleted and the rows of libraries whose folder \
         is gone are pruned whatever refreshLibrary says; upstream runs \
         ValidateTopLibraryFolders(removeRoot: true) only with refreshLibrary, so without it the \
         removed library's row and view stay until the next scan. Jellyfin bug not ported.",
            "With refreshLibrary the monitor is restarted too, and the full scan is queued behind a \
         running one; upstream leaves the restart to the scan's ValidateMediaLibraryInternal \
         (Ferrofin's scan does not restart the monitor) and drops the scan when one is running \
         (StartScanInBackground), which misses the removal when that scan listed the libraries \
         before it. Jellyfin bug not ported.",
            "A change the monitor was still settling when the restart came is refreshed after it; \
         upstream's Stop disposes its FileRefreshers, so that change waits for the next scan. \
         Jellyfin bug not ported.",
            "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Items/Filters2",
        upstream_file: "Jellyfin.Api/Controllers/FilterController.cs",
        upstream_method: "GetQueryFilters",
        ferrofin: "handlers/filter.rs get_query_filters (items.rs resolve_user_opt for \
               RequestHelpers.GetUserId and GetUserById, an id naming no account leaving no \
               user; is_trailer_or_program skipping GetItemById; the nil parentId's 400 and \
               require_scope_visible; parent_scope for the AncestorIds-or-Parent choice, Parent \
               as physical_children_only's strict ParentId; is_music_type_set, \
               stream_language_types, has_language_facets, and language_pairs for \
               FindLanguageInfo and OrderBy(Name) under CurrentCulture) → ferrofin-core \
               item_repository.rs get_genres / get_music_genres (item_values_with_counts, as \
               the /Genres and /MusicGenres rows), get_tag_names (BaseItemRepository.GetTagNames: \
               item_values_statement) and get_media_stream_languages_by_type \
               (GetMediaStreamLanguages: stream_language_query, stream_languages_statement, one \
               statement for both stream types), each after top_parent_or_ancestor_scope \
               (LibraryManager.SetTopParentOrAncestorIds) and AddUserToQuery \
               (scope_to_user_libraries, through resolve_views on the tag and language \
               facets); translate_query.rs append_predicates for the TranslateQuery fields these \
               queries set, which no query-translation row covers yet (gl-1769)",
        date: "2026-11-14",
        divergences: &[
            "The 400 (a nil parentId, or a value the query binder cannot convert) and 403 \
             (another user's id without the administrator role) bodies are Ferrofin's error \
             shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["access-filtering"],
    },
    Verified {
        method: "post",
        path: "/Library/VirtualFolders/Name",
        upstream_file: "Jellyfin.Api/Controllers/LibraryStructureController.cs",
        upstream_method: "RenameVirtualFolder",
        ferrofin: "handlers/library_structure.rs rename_virtual_folder (the name and newName 400s) \
               → library_id_by_name for the scan's scope → finish_structure_change \
               (after_structure_change: LibraryMonitor::restart, then queue_library_scan_scoped \
               when refreshLibrary) → ferrofin-core virtual_folder_manager.rs \
               rename_virtual_folder (child_path for GetChildPath, the 404 and 409 checks, \
               case_hop for the case-only temp hop, the preference remap for \
               ReplaceLibraryIdInUserPreferencesAsync over library_preference_repository.rs, \
               repath_collection_folder and prune_orphan_collection_folders) → \
               library_monitor.rs restart and run (LibraryMonitor.Stop and Start)",
        date: "2026-11-03",
        divergences: &[
            "The monitor keeps watching while the folder moves and restarts after; upstream stops \
         it first. The move happens under the libraries root, which is not a library location, \
         so no event of it reaches the monitor.",
            "Without refreshLibrary the restart runs at once on the monitor's own task; upstream's \
         RefreshAfterRenameAsync waits 1000 ms (Task.Delay) first, which for the reason above \
         has nothing to let settle here.",
            "With refreshLibrary a scan scoped to the renamed library is queued and the monitor is \
         restarted; upstream refreshes the library's physical folders directly on a background \
         task, beside any running scan, and never calls Start, so real-time monitoring stays off \
         until the next full scan. Jellyfin bug not ported (the missing restart).",
            "Without refreshLibrary the old library's row is repathed or deleted and orphan rows \
         pruned at once, and the listing writes the new row; upstream validates the top folders \
         only with refreshLibrary, so the old row stays while users' preferences already name the \
         new id. Jellyfin bug not ported.",
            "A case-only rename onto another library's spelling on a case-sensitive filesystem is a \
         409, and a failed second hop moves the folder back; upstream skips the conflict check \
         for a case-only rename, answers 500 from the second move and leaves the folder under its \
         temporary name. Jellyfin bug not ported.",
            "A newName that names a regular file in the libraries root is a 409; upstream's \
         Directory.Exists misses the file and the move fails with 500.",
            "When the preference remap fails after the move, the requested scan is a full one, \
         since no settled id scopes it; upstream still refreshes the renamed library.",
            "A change the monitor was still settling when the restart came is refreshed after it; \
         upstream's Stop disposes its FileRefreshers. Jellyfin bug not ported.",
            "The 400, 404 and 409 bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Library/VirtualFolders/Paths",
        upstream_file: "Jellyfin.Api/Controllers/LibraryStructureController.cs",
        upstream_method: "AddMediaPath",
        ferrofin: "handlers/library_structure.rs add_media_path (the Name and PathInfo/Path 400s, \
               PathInfo preferred over Path) → finish_structure_change (after_structure_change: \
               LibraryMonitor::restart, then queue_library_scan when refreshLibrary) → \
               ferrofin-core virtual_folder_manager.rs add_media_path \
               (LibraryManager.AddMediaPath and AddMediaPathInternal: the path's existence, \
               require_folder, create_shortcut for CreateShortcut, \
               remove_content_type_overrides, sync_options_to_locations for \
               SyncLibraryOptionsToLocations, save_options, record_changed_locations) → \
               library_monitor.rs restart and run (LibraryMonitor.Stop and Start)",
        date: "2026-11-03",
        divergences: &[
            "A request refused for its arguments or for a missing library or path (400, 404) \
         restarts nothing and queues no scan; upstream makes those checks inside its try, so its \
         finally still queues the requested full scan, or restarts the monitor, for a library it \
         did not change.",
            "The monitor keeps watching while the shortcut is written and restarts after it, at \
         once; upstream stops it first and starts it again after 1000 ms (Task.Delay). The \
         shortcut is written under the libraries root, which is not a library location, so no \
         event of it reaches the monitor and there is nothing for the wait to let settle.",
            "With refreshLibrary the monitor is restarted as well as the full scan queued; upstream \
         leaves the restart to the scan's ValidateMediaLibraryInternal, and Ferrofin's scan does \
         not restart the monitor.",
            "A change the monitor was still settling when the restart came is refreshed after it; \
         upstream's Stop disposes its FileRefreshers. Jellyfin bug not ported.",
            "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Library/VirtualFolders/Paths/Update",
        upstream_file: "Jellyfin.Api/Controllers/LibraryStructureController.cs",
        upstream_method: "UpdateMediaPath",
        ferrofin: "handlers/library_structure.rs update_media_path (the Name and PathInfo 400s; \
               no monitor restart, as upstream's) → ferrofin-core virtual_folder_manager.rs \
               update_media_path (LibraryManager.UpdateMediaPath: require_folder, load_options, \
               sync_options_to_locations for SyncLibraryOptionsToLocations, save_options, \
               record_changed_locations; the posted PathInfo is not applied, as upstream's is not) \
               → ferrofin-model configuration.rs MediaPathInfo (a missing Path binds as \"\", \
               as MediaPathInfo() sets it)",
        date: "2026-11-14",
        divergences: &[
            "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "delete",
        path: "/Library/VirtualFolders/Paths",
        upstream_file: "Jellyfin.Api/Controllers/LibraryStructureController.cs",
        upstream_method: "RemoveMediaPath",
        ferrofin: "handlers/library_structure.rs remove_media_path (the name and path 400s) → \
               finish_structure_change (after_structure_change: LibraryMonitor::restart, then \
               queue_library_scan when refreshLibrary) → ferrofin-core virtual_folder_manager.rs \
               remove_media_path (LibraryManager.RemoveMediaPath: require_folder, \
               delete_shortcut_for (every match) over resolve_shortcut and the virtual-path \
               expansion, compared with the path ignoring case, the ordinal PathInfos filter, \
               both past trailing separators, save_options, record_changed_locations) → \
               library_monitor.rs restart and run (LibraryMonitor.Stop and Start)",
        date: "2026-11-17",
        divergences: &[
            "The path is compared with each shortcut's target and each PathInfos entry past \
         their trailing separators, and every matching shortcut is deleted, as every matching \
         entry is, so no shortcut outlives its entry: a folder added with a trailing slash and \
         removed with that stored path, as jellyfin-web sends it, loses its shortcut and its \
         entry; a path that differs from the stored one only by trailing slashes removes both; \
         a folder added twice loses both shortcuts. Upstream compares the raw path with both (a \
         shortcut's target loses one trailing separator) and deletes the first matching \
         shortcut, so the first case drops the entry but keeps the shortcut, and the folder is \
         still scanned; a path stored with a slash and requested without one loses only its \
         shortcut; one stored without and requested with one changes nothing; a folder added \
         twice keeps a shortcut. Jellyfin bug not ported (gl-1871).",
            "A request for a library that does not exist (404) restarts nothing and queues no \
         scan; upstream throws inside its try, so its finally still queues the requested full \
         scan, or restarts the monitor, for an unchanged library.",
            "Shortcuts are looked for in the library folder itself; upstream also searches its \
         subfolders (GetFilePaths with recursion), where neither server writes one.",
            "The monitor keeps watching while the shortcut is deleted and restarts after it, at \
         once; upstream stops it first and starts it again after 1000 ms (Task.Delay). The \
         shortcut lives under the libraries root, which is not a library location, so no event \
         of it reaches the monitor and there is nothing for the wait to let settle.",
            "With refreshLibrary the monitor is restarted as well as the full scan queued; upstream \
         leaves the restart to the scan's ValidateMediaLibraryInternal, and Ferrofin's scan does \
         not restart the monitor.",
            "A change the monitor was still settling when the restart came is refreshed after it; \
         upstream's Stop disposes its FileRefreshers. Jellyfin bug not ported.",
            "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Library/VirtualFolders/LibraryOptions",
        upstream_file: "Jellyfin.Api/Controllers/LibraryStructureController.cs",
        upstream_method: "UpdateLibraryOptions",
        ferrofin: "handlers/library_structure.rs update_library_options (Name, else Id resolved \
               against the listing's parsed ItemId; 400 for a missing, null or all-zero Id, as \
               GetItemById(Guid.Empty) throws; 404 for an Id naming no library; then 400 for a \
               missing or null LibraryOptions, before any write) → after_structure_change \
               (LibraryMonitor::restart) → ferrofin-core virtual_folder_manager.rs \
               update_library_options (CollectionFolder.UpdateLibraryOptions: an empty or blank \
               new path refused first, then create_shortcut and remove_content_type_overrides \
               for each path not already in PathInfos, then save_options of the posted options, \
               record_changed_locations) → library_monitor.rs restart and run; ferrofin-model \
               configuration.rs MediaPathInfo (a missing Path binds as \"\")",
        date: "2026-11-14",
        divergences: &[
            "The library monitor is asked to restart after the save, so an EnableRealtimeMonitor \
         toggle or a new location is watched at once; upstream's UpdateLibraryOptions does not \
         touch the monitor, so the dashboard's real-time monitoring checkbox applies only at the \
         next library scan. Jellyfin bug not ported.",
            "A body without LibraryOptions (or with null) is a 400, \
         {\"error\":\"bad request: LibraryOptions must not be null\"}, after the library lookup \
         and before any write; upstream's request.LibraryOptions! dereference throws a \
         NullReferenceException at the same point, a 500 having written nothing. A 400 names \
         the field the client must send; a 500 says the server failed.",
            "A new PathInfo whose Path is empty or blank is a 400 before any shortcut is written. \
         Upstream also answers 400 without saving the options, but it throws inside its \
         CreateShortcut loop (ManagedFileSystem.CreateShortcut on an empty target, \
         RemoveContentTypeOverrides on a blank path after writing its shortcut), so the \
         shortcuts of the new paths before it, and a blank path's own, stay in the library \
         folder and become its locations. Jellyfin bug not ported.",
            "A Name in the body is accepted and wins over Id; UpdateLibraryOptionsDto has no Name.",
            "The 400 and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/DisplayPreferences/{displayPreferencesId}",
        upstream_file: "Jellyfin.Api/Controllers/DisplayPreferencesController.cs",
        upstream_method: "GetDisplayPreferences",
        ferrofin: "handlers/display_preferences.rs get_display_preferences \
               (RequiredRouteString displayPreferencesId and Query: ConvertEmptyStringToNull, \
               then [Required], client from query/required.json; \
               effective_user_id: RequestHelpers.GetUserId; resolve_item_id: Guid.TryParse \
               else GetMD5; index_by_name, chromecast_name, home_section_name) → ferrofin-core \
               display_preferences_manager.rs (get_display_preferences, \
               get_item_display_preferences, list_custom_item_display_preferences, \
               list_home_sections) → ferrofin-db database.rs home_sections (Order, then Id: \
               the Include(HomeSections) load order); ferrofin-model \
               dto/display_preferences_dto.rs",
        date: "2026-10-30",
        divergences: &[
            "CustomPrefs members are written in hash order, not upstream's insertion order \
         (the home sections, the scalar keys, then the stored preferences); the members and \
         values are the same.",
            "The 400 for a missing, empty or whitespace-only client or an unparsable userId \
         and the 403 for another user's id without the administrator role are Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/DisplayPreferences/{displayPreferencesId}",
        upstream_file: "Jellyfin.Api/Controllers/DisplayPreferencesController.cs",
        upstream_method: "UpdateDisplayPreferences",
        ferrofin: "handlers/display_preferences.rs update_display_preferences \
               (RequiredRouteString displayPreferencesId and Query: ConvertEmptyStringToNull, \
               then [Required], client from query/required.json; \
               JsonBody DisplayPreferencesDto, resolve_item_id: Guid.TryParse else GetMD5, \
               take_pref/take_nullable_pref, enum_try_parse for Enum.TryParse/Enum.Parse, \
               parse_dotnet_int/parse_dotnet_bool for int.Parse/bool.Parse) → ferrofin-core \
               display_preferences_manager.rs (get_display_preferences, \
               get_item_display_preferences, set_custom_item_display_preferences, \
               update_item_display_preferences, update_display_preferences, \
               set_home_sections) → ferrofin-db database.rs replace_home_sections; \
               ferrofin-model dto/display_preferences_dto.rs",
        date: "2026-10-30",
        divergences: &[
            "Values upstream fails on with a 500 take the defaults here: an \
         enableNextVideoInfoOverlay that is not true/false (bool.Parse) is true, a \
         non-numeric or overflowing skip length (int.Parse) is the fallback, a homesection \
         key with a non-numeric suffix is dropped, a bare homesection key stays a custom \
         pref, a negative order falls back to None instead of indexing defaults[-1], and a \
         chromecastVersion integer outside Int32 is a 400 (Enum.Parse's OverflowException \
         is a 500 there). Jellyfin bug not ported.",
            "Every save replaces the row's HomeSection children. Upstream clears the \
         collection of an entity loaded in a disposed context and attaches it to a new \
         one, so EF inserts the new sections but never deletes the old rows: they pile up, \
         and a save without any homesection key leaves the previous layout visible to GET. \
         Jellyfin bug not ported.",
            "An invalid landing-* ViewType is logged at warn, as bad client input \
         (docs/conventions/LOGGING.md), where upstream logs it at error.",
            "The 400 for a missing, empty or whitespace-only client, an unparsable userId or \
         a chromecastVersion Enum.Parse refuses, the 403 for another user's id without the \
         administrator role, and the error text of a body that does not bind are \
         Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Playlists",
        upstream_file: "Jellyfin.Api/Controllers/PlaylistsController.cs",
        upstream_method: "CreatePlaylist",
        ferrofin: "handlers/playlists.rs create_playlist (CreatePlaylistQuery, effective_user_id) \
               → ferrofin-model playlists.rs CreatePlaylistDto (Ids through \
               json::guid::comma_delimited, JsonCommaDelimitedCollectionConverterFactory) \
               → ferrofin-core \
               collection_manager.rs FerrofinPlaylistManager::create_playlist \
               (PlaylistManager.CreatePlaylist: playlists_folder → item_persistence_service \
               playlists_folder_by_type, GetPlaylistsFolder's by-type lookup; \
               infer_media_type (a folder through first_member_media_type, \
               GetRecursiveChildren's AddChildrenToList walk over get_folder_children \
               and the folder's LinkedChildren), require_user, \
               new_playlist_folder_and_id, its Path stored through \
               VirtualPathExpander::reverse as AddChild's save stores it, \
               %AppDataPath%/playlists/<name> (BaseItemMapper.Map → GetPathToSave → \
               ReverseVirtualPath), then report_item_added for AddChild's \
               ItemAdded; playlist_items for AddToPlaylistInternal and \
               Playlist.GetPlaylistItems)",
        date: "2026-11-03",
        divergences: &[
            "A request with neither a body nor a name query is a 400; upstream's \
         GetValidFilename(null) throws a NullReferenceException (500). Jellyfin bug not \
         ported.",
            "An empty-GUID id (`\"Ids\":[null]` or all zeros) with a media type named is \
         dropped and the playlist is created (200); upstream's GetItemById throws \
         ArgumentException inside AddToPlaylistInternal after AddChild, so it answers 400 \
         and leaves an empty playlist behind. Jellyfin bug not ported.",
            "A folder name taken by a file, or whose derived id an existing row owns, moves on \
         to the next `1`-suffixed folder; upstream's GetTargetPath checks only \
         Directory.Exists, so a file makes CreateDirectory throw (500) and a row whose \
         folder went missing is saved over. Jellyfin bug not ported.",
            "The new playlist's directory is made in {data}/playlists, where upstream \
         combines the found folder's Path; they differ only for a folder found by type away \
         from that directory, since startup keeps the folder in use there \
         (AggregateFolderStore::ensure).",
            "The first ForceSave refresh writes no playlist.xml into the new folder, where \
         upstream's PlaylistXmlSaver saves one. Fix path: gl-341.",
            "With no media type named, a folder whose tree-order walk finds no playable \
         member (a box set of series or video folders, whose linked folders \
         GetRecursiveChildren never looks inside; a season whose episodes are stored under \
         the series, so its Children are empty) answers with the first member of its flat \
         recursive listing, read without a user; upstream moves on to the next id, and \
         types the playlist Audio when none answers, whatever its entries. Jellyfin bug \
         not ported.",
            "A malformed id in the ids query is a 400; upstream's \
         CommaDelimitedCollectionModelBinder drops it, so an all-malformed list falls back \
         to the body's Ids. Fix path: gl-884.",
            "A user named twice in Users keeps one share, at their first entry's place, that \
         can edit when any of their entries can: the permission upstream grants \
         (Shares.Any(s => s.CanEdit && …)). Upstream stores and lists every entry, so GET \
         /Playlists/{id}/Users lists such a user once here, GET \
         /Playlists/{id}/Users/{userId} answers the combined CanEdit where upstream's \
         FirstOrDefault answers the first entry's, and DELETE \
         /Playlists/{id}/Users/{userId} removes the user's one share where upstream \
         removes only the first entry (a second one keeps sharing). Fix path: gl-949.",
            "The 400 (no name, no playlists folder, an unknown id while the media type is \
         inferred, an API key that names no user) and 403 (another user's id without the \
         administrator role) bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Playlists/{itemId}",
        upstream_file: "Jellyfin.Api/Controllers/PlaylistsController.cs",
        upstream_method: "GetPlaylist",
        ferrofin: "handlers/playlists.rs get_playlist (User.GetUserId: a nil id, an API key \
               naming no user, is a 400 as GetUserById(Guid.Empty) throws) → ferrofin-core \
               collection_manager.rs FerrofinPlaylistManager::get_playlist_access \
               (GetPlaylistForUser: GetPlaylists(userId).Where(IsVisible), else 404), \
               get_playlist_shares (Playlist.Shares) and get_playlist_item_ids \
               (GetManageableItems().Select(t => t.Item2.Id): the manual links in SortOrder, \
               repeats kept, every member listed whoever may see it, read as ids alone)",
        date: "2026-11-09",
        divergences: &[
            "A user named twice in the shares is listed once, with the combined CanEdit; \
         upstream's Shares lists every stored entry. Fix path: gl-949.",
            "The 404 (no playlist the caller can see) body is Ferrofin's error shape, not \
         upstream's \"Playlist not found\" string. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Playlists/{itemId}",
        upstream_file: "Jellyfin.Api/Controllers/PlaylistsController.cs",
        upstream_method: "UpdatePlaylist",
        ferrofin: "handlers/playlists.rs update_playlist (get_playlist_access + require_edit: \
               GetPlaylistForUser and the owner-or-editing-share check) → ferrofin-model \
               playlists.rs UpdatePlaylistDto (Ids through \
               json::guid::comma_delimited::option, the same converter), \
               entities_media.rs PlaylistUserPermissions \
               (constructor defaults) → ferrofin-core collection_manager.rs \
               FerrofinPlaylistManager::update_playlist (PlaylistManager.UpdatePlaylist: \
               playlist_items for the Ids' AddToPlaylistInternal, rename_named_item_in, \
               OpenAccess, share_playlist_in, sync_container_data_in, queue_refresh)",
        date: "2026-10-28",
        divergences: &[
            "An empty-GUID id in Ids (`\"Ids\":[null]` or all zeros) is dropped and the other \
         ids replace the entries (204); upstream's UpdatePlaylist clears the entries and \
         saves, then GetItemById throws ArgumentException inside AddToPlaylistInternal, so \
         it answers 400 and leaves the playlist empty. Jellyfin bug not ported.",
            "Ids, Name, Users and IsPublic are saved in one transaction with the Data sync; \
         upstream saves the cleared entries before it adds the new ones and then saves the \
         rest, so a failure part way leaves a partial update. Jellyfin bug not ported.",
            "An Ids entry naming a container that holds this playlist expands that container \
         with this playlist's entries from before the update; upstream clears them first, so \
         it expands without them. The playlist's own id expands to nothing on both.",
            "When the entries change, the members are re-rated before the refresh is queued; \
         upstream re-rates them in that refresh (MetadataService's UpdateRatingToItems). The \
         ratings end the same.",
            "SavePlaylistFile has no caller: a playlist file the scan found has no owner and \
         no editing share, so an update of it is a 403 here as upstream, and no update \
         reaches a file.",
            "A user named twice in Users keeps one share, at their first entry's place, that \
         can edit when any of their entries can: the permission upstream grants \
         (Shares.Any(s => s.CanEdit && …)). Upstream stores and lists every entry, so GET \
         /Playlists/{id}/Users lists such a user once here, GET \
         /Playlists/{id}/Users/{userId} answers the combined CanEdit where upstream's \
         FirstOrDefault answers the first entry's, and DELETE \
         /Playlists/{id}/Users/{userId} removes the user's one share where upstream \
         removes only the first entry (a second one keeps sharing). Fix path: gl-949.",
            "The 404 (no playlist the caller can see) and 403 (no editing share) bodies are \
         Ferrofin's error shape, not upstream's (a \"Playlist not found\" string and an \
         empty Forbid). Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Playlists/{itemId}/Items",
        upstream_file: "Jellyfin.Api/Controllers/PlaylistsController.cs",
        upstream_method: "GetPlaylistItems",
        ferrofin: "handlers/playlists.rs get_playlist_items (effective_user_id: \
               RequestHelpers.GetUserId) → ferrofin-core collection_manager.rs \
               FerrofinPlaylistManager::get_playlist_items (GetPlaylistForUser and \
               GetManageableItems: manual links, in order, missing items out, each distinct \
               member's row read once and its entries pointing at it, as \
               ResolveLinkedChildren's byId lookup shares one BaseItem between repeats), \
               then LibraryManager::filter_visible_items_standalone over those rows \
               (ItemVisibility::visible, one answer per distinct id) before the paging, as \
               upstream filters before Skip and Take (the count taken first; a negative \
               startIndex skips nothing, a negative limit takes nothing), and the page's \
               entries through tv_shows::build_dto_options + get_linked_children_dtos + the \
               PlaylistItemId tag",
        date: "2026-11-11",
        divergences: &[
            "A member the caller may not open is left out with the standalone check, which also \
         walks its library, where upstream's IsVisible reads only its rating and tags and \
         lists a member from a library the caller was not granted. An accepted divergence \
         (owner decision 2026-10-31, gl-1092): only this read and Move's index change, so \
         the stored members, Add, Create and ItemIds stay as upstream has them. It is the \
         check GET /Items?ids= uses. GET /Items?ParentId=<playlist> takes \
         Folder.GetLinkedChildren(user)'s rule (item_visibility.rs folder_children), which \
         agrees on a member of a library the caller was or was not granted and differs at \
         the edges: a member of a disabled library or a file in no library is kept here and \
         dropped there, and a member whose parent fails the caller's rating limit is dropped \
         here and kept there. This list answers as GET /Items/{id} does.",
            "The 404 (no playlist the caller can see) and 403 (another user's id without the \
         administrator role) bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &["access-filtering"],
    },
    Verified {
        method: "post",
        path: "/Playlists/{itemId}/Items/{itemId}/Move/{newIndex}",
        upstream_file: "Jellyfin.Api/Controllers/PlaylistsController.cs",
        upstream_method: "MoveItem",
        ferrofin: "handlers/playlists.rs move_item (blank_route_values playlistId and \
               itemId: ConvertEmptyStringToNull, then [Required], both in one 400; the \
               playlist id parses through \
               DotnetGuid::parse, Guid.Parse and MoveItemAsync's new Guid; \
               get_playlist_access + require_edit: \
               GetPlaylistForUser and the owner-or-editing-share check) → ferrofin-core \
               collection_manager.rs FerrofinPlaylistManager::move_item \
               (PlaylistManager.MoveItemAsync: the item id names an entry only in its N \
               form, either case, as the string compare with ItemId.ToString(\"N\") \
               ignoring case does; the first occurrence moves, the index counts the \
               caller's list, no write when it is already there) and visible_entries",
        date: "2026-11-14",
        divergences: &[
            "The index counts the members GET /Playlists/{id}/Items gave the caller, so the \
         standalone check, library included, where upstream's accessibleChildren take \
         IsVisible alone. An accepted divergence (owner decision 2026-10-31, gl-1092). A \
         hidden member named by its id still moves, as upstream: its id comes from ItemIds, \
         not the listing.",
            "Moving down past a member hidden from the caller lands at the index asked for; \
         upstream anchors on the list before the entry is taken out \
         (DetermineAdjustedIndex) and lands one place short. Jellyfin bug not ported.",
            "An index beyond the length of the caller's list goes to the end; upstream's \
         accessibleChildren[newPriorItemIndex] throws (500), as it does at any index when \
         the caller's list is empty. It throws before it looks the entry up, so an item id \
         that names no entry is a 500 there at such an index, and a 204 that moves nothing \
         here. Jellyfin bug not ported.",
            "A malformed playlist id is a 400; upstream's Guid.Parse throws FormatException, \
         which ExceptionMiddleware answers with a 500. Jellyfin bug not ported.",
            "The 400 (a malformed index), 403 (no editing share) and 404 (no playlist the \
         caller can see) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["access-filtering"],
    },
    Verified {
        method: "post",
        path: "/Playlists/{itemId}/Users/{userId}",
        upstream_file: "Jellyfin.Api/Controllers/PlaylistsController.cs",
        upstream_method: "UpdatePlaylistUser",
        ferrofin: "handlers/playlists.rs update_playlist_user (the body through \
               Option<JsonBody>, as [FromBody(EmptyBodyBehavior = EmptyBodyBehavior.Allow), \
               Required] binds it: an empty body whatever the content type, or null with a \
               JSON one, binds null and is [Required]'s 400 keyed \"\" before the action; \
               with a JSON content type a blank or malformed body is the binder's 400 with \
               the parameter's entry, and any non-empty body with another content type is \
               the 415; get_playlist_access: GetPlaylistForUser for the caller, a nil \
               caller id a 400; the owner check's Forbid) → ferrofin-model playlists.rs \
               UpdatePlaylistUserDto (CanEdit bool?) → ferrofin-core collection_manager.rs \
               FerrofinPlaylistManager::add_user_to_shares (PlaylistManager.AddUserToShares: \
               require_user, GetUserById(Guid.Empty)'s 400 for a nil target; \
               item_data::share_playlist_in removes the user's share and appends one with \
               CanEdit ?? false; the Data sync and announce_updated for \
               UpdateToRepositoryAsync. UpdatePlaylistInternal's SavePlaylistFile is \
               unreachable on both: a scanned playlist file has no owner, so the owner check \
               refuses it first)",
        date: "2026-11-15",
        divergences: &[
            "A target user the playlist is hidden from (a private playlist not shared with \
         them, a parental rating limit no member passes) or one that does not exist is given \
         the share. Upstream's AddUserToShares looks the playlist up for the target user \
         (GetPlaylistForUser(request.Id, userId)), finds none and throws \
         NullReferenceException, so it answers 500 and writes nothing: only a user who can \
         already see the playlist can be added there (measured on Jellyfin 12.4.0 for a \
         private playlist and an unknown id). Jellyfin bug not ported.",
            "UpdateToRepositoryAsync runs the metadata savers, and PlaylistXmlSaver (enabled \
         for a Playlist at MetadataEdit) rewrites the playlist folder's playlist.xml with the \
         new Shares (measured on Jellyfin 12.4.0); Ferrofin writes no playlist.xml. Fix path: \
         gl-341.",
            "A user named twice in the shares has one entry here, which a re-share replaces; \
         upstream removes only the first of their entries and appends the new one, so the \
         second keeps its permission. Fix path: gl-949.",
            "The 400 (a nil target user id, an API key naming no user), 403 (not the owner) \
         and 404 (no playlist the caller can see) bodies are Ferrofin's error shape, not \
         upstream's (\"Error processing request.\", an empty Forbid and a \"Playlist not \
         found\" string). Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Environment/DirectoryContents",
        upstream_file: "Jellyfin.Api/Controllers/EnvironmentController.cs",
        upstream_method: "GetDirectoryContents",
        ferrofin: "handlers/environment.rs get_directory_contents → ferrofin-core file_system.rs \
               try_get_file_system_entries",
        date: "2026-10-30",
        divergences: &[
            "The 400 (no path, or a flag that is no boolean), 404 (the path is no directory) and \
         500 (the listing failed) bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Environment/ValidatePath",
        upstream_file: "Jellyfin.Api/Controllers/EnvironmentController.cs",
        upstream_method: "ValidatePath",
        ferrofin: "handlers/environment.rs validate_path → ferrofin-core file_system.rs \
               file_exists, directory_exists, validate_writable",
        date: "2026-10-30",
        divergences: &[
            "The 400 (no body) and the writable check's 404 and 500 bodies are Ferrofin's error \
         shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Environment/Drives",
        upstream_file: "Jellyfin.Api/Controllers/EnvironmentController.cs",
        upstream_method: "GetDrives",
        ferrofin: "handlers/environment.rs get_drives → ferrofin-core file_system.rs get_drives, \
               drive_probe.rs, mount_table.rs (read_mount_table for DriveInfo.GetDrives(): \
               /proc/mounts, or on Windows GetLogicalDrives' roots with GetDriveTypeW; IsReady \
               as Directory.Exists(Name); disk_space for TotalSize, statvfs or \
               GetDiskFreeSpaceExW; Name and FullName the mount point or C:\\)",
        date: "2026-11-15",
        divergences: &[
            "A mount that does not answer in time (drive_probe.rs DRIVE_PROBE_TIMEOUT) is left \
         out of that listing, a hung one and one merely that slow, where upstream's request \
         waits for it for as long as the mount takes. Deliberate (gl-26): upstream's thread \
         pool grows, and a hung statvfs cannot be cancelled.",
            "With no readable mount table, or no mount that qualifies, the list is `/`; \
         upstream's is empty.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/System/Logs",
        upstream_file: "Jellyfin.Api/Controllers/SystemController.cs",
        upstream_method: "GetServerLogs",
        ferrofin: "handlers/system.rs get_server_logs → ferrofin-core file_system.rs get_files",
        date: "2026-10-30",
        divergences: &[],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/System/Logs/Log",
        upstream_file: "Jellyfin.Api/Controllers/SystemController.cs",
        upstream_method: "GetLogFile",
        ferrofin: "handlers/system.rs get_log_file → ferrofin-core file_system.rs get_files, \
               open_file → handlers/streaming.rs opened_file_response",
        date: "2026-10-30",
        divergences: &[
            "A log that grows while it is read stops at the length the headers declared; \
         upstream copies to the end of the stream, past that length, which Kestrel aborts. \
         Jellyfin bug not ported.",
            "The 400 (no name), 404 (no such log, or one gone before it was opened) and 500 (one \
         that cannot be opened) bodies are Ferrofin's error shape, not upstream's (a \
         \"Log file not found.\" string for the first 404). Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/System/Endpoint",
        upstream_file: "Jellyfin.Api/Controllers/SystemController.cs",
        upstream_method: "GetEndpointInfo",
        ferrofin: "handlers/system.rs get_endpoint_info (auth.rs RequireAuth) → state.rs \
               client_address (GetNormalizedRemoteIP after ForwardedHeadersMiddleware) → \
               ip_access.rs is_local_connection (HttpContextExtensions.IsLocal), state.rs \
               is_in_local_network (NetworkManager.IsInLocalNetwork)",
        date: "2026-11-10",
        divergences: &[
            "IsLocal compares the connection's local address and the client's with \
         IPv4-mapped IPv6 addresses unmapped, as the IP gate does; upstream's Equals compares \
         them as given, so a dual-stack listener's mapped local address never equals a plain \
         IPv4 client that a known proxy forwarded. Jellyfin bug not ported.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/System/Restart",
        upstream_file: "Jellyfin.Api/Controllers/SystemController.cs",
        upstream_method: "RestartApplication",
        ferrofin: "handlers/system.rs restart_application (auth.rs RequireAdmin) → ferrofin-core \
               system_manager.rs restart → apps/ferrofin-server state.rs \
               FerrofinLifecycleController stop (SystemManager.ShutdownInternal: the restart \
               decision, ShouldRestart, with HasPendingRestart left alone), lib.rs \
               announce_shutdown (SessionManager.OnApplicationStopping), tear_down_host \
               (HlsStreamManager::shutdown, AttachmentExtractor::shutdown, \
               LiveTvManager::shutdown: Program.cs disposes the host, RecordingsManager.Dispose, \
               AttachmentExtractor.Dispose, MediaSourceManager.Dispose)",
        date: "2026-11-15",
        divergences: &[
            "The host teardown goes further than upstream's disposal (gl-141). It does five things. \
         It kills every transcode: upstream's TranscodeManager.Dispose kills no job, and each \
         dies at its own kill timer, into a transcode cache the next host has already purged. \
         It kills every attachment extraction still running and deletes the outputs it names \
         (gl-1517; a batch dump names none, as its files are named by the media): \
         upstream's AttachmentExtractor.Dispose only disposes its locks, and an extraction, \
         which runs with a token nothing cancels, finishes under the next host, whose fresh \
         locks let a request serve the half-written file or start a second ffmpeg onto it. \
         It closes every open live stream: MediaSourceManager.Dispose releases one consumer \
         per stream. It stops the pending recording timers: upstream does not dispose its \
         TimerManager. And it waits for the recordings RecordingsManager.Dispose cancels to \
         settle, which upstream does not, because here they settle into a database the host \
         closes next. No client can tell: the drain has already disconnected them all.",
            "Stricter than upstream by owner decision (gl-1103), not a gap to port: an \
         administrator or API key is required whatever the source address, as on \
         /System/Shutdown. Upstream's LocalAccessOrRequiresElevation also admits any caller \
         on the local network, with or without a token; here that caller gets 401 without \
         a token and 403 without the administrator role.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/System/Info",
        upstream_file: "Jellyfin.Api/Controllers/SystemController.cs",
        upstream_method: "GetSystemInfo",
        ferrofin: "handlers/system.rs get_system_info (auth.rs FirstTimeSetupOrAuth: \
               FirstTimeSetupOrIgnoreParentalControl, FirstTimeSetupHandler then \
               DefaultAuthorizationHandler with no parental schedule: anonymous while the \
               startup wizard is incomplete, where a valid token is still held to its user's \
               remote access, and once complete 401 without a valid token or for a disabled \
               user and 403 for a user without remote access from outside the LAN) → \
               context_from_parts (the scheme and Host ForwardedHeadersMiddleware gives the \
               request, and its client address) → ferrofin-core system_manager.rs \
               get_system_info (SystemManager.GetSystemInfo) → application_host.rs \
               get_smart_api_url (GetSmartApiUrl(HttpRequest): EnablePublishedServerUriByRequest \
               read live, the request's Host with no port for the scheme's own default; else \
               the startup published URL, then ferrofin-networking manager.rs \
               get_bind_address_for_peer, GetBindAddress: the subnet overrides, the address a \
               listener pinned at startup is bound to, as the one interface a \
               LocalNetworkAddresses bind leaves upstream, then the interfaces) and \
               format_api_url (GetLocalApiUrl's UriBuilder, which writes any port it is given \
               and escapes the base URL as UriBuilder.Path does, a backslash aside: \
               ferrofin-networking escape_base_url, checked against 12.4's discovery Address; \
               its default scheme from listen_with_https, ListenWithHttps: the certificate \
               this lifetime loaded and the saved EnableHttps, read on every call), \
               friendly_name (FriendlyName: the saved ServerName unless empty, else the \
               server_name setting, whose default, config.rs default_server_name, is \
               application_host.rs machine_name for Environment.MachineName: gethostname cut at \
               its first dot, the computer name (its COMPUTERNAME variable) on Windows, \
               ferrofin when there is none), the \
               emulated version, PRODUCT_NAME (Name), the system id (DeviceId), the \
               startup-wizard flag, the lifecycle's restart-pending and shutting-down flags \
               (HasPendingRestart, NotifyPendingRestart's flag, raised by a change that \
               takes a restart, among them backup.rs schedule_restore before it restarts, and \
               by no stop; IsShuttingDown, ApplicationStopping, set by a stop or by a SIGTERM \
               or SIGINT in lib.rs announce_shutdown), the bound HTTP \
               port (HttpPort), CompletedInstallations from plugin_manager.rs \
               completed_installations (InstallationManager.CompletedInstallations: the \
               InstallationInfo GetCompatibleVersions built for each package install_package \
               staged since the boot, by POST /Packages/Installed/{name} or the Update Plugins \
               task, a failed one left out, in install order where upstream's ConcurrentBag \
               keeps none), the paths (ProgramDataPath, WebPath, LogDirectoryPath, \
               InternalMetadataPath for both ItemsByNamePath and InternalMetadataPath, \
               CachePath) and the live transcode path prepared as GetTranscodePath's \
               CreateAndCheckMarker (a 500 when it cannot be), CastReceiverApplications \
               (seeded with AddDefaultCastReceivers' two receivers, an adopted system.xml's \
               kept), SupportsLibraryMonitor true and the obsolete members' fixed values. \
               Probed on Jellyfin 12.4: LocalAddress for a published override naming port 80, \
               ServerName with no name saved, the body with the wizard incomplete",
        date: "2026-11-15",
        divergences: &[
            "Version is the Jellyfin release Ferrofin emulates (openapi.rs \
         JELLYFIN_API_VERSION, 12.4.0), not Ferrofin's own release version, which upstream's \
         field would carry. Deliberate: clients gate on it (jellyfin-web refuses a server \
         below 10.10.0, the Kotlin SDK flags one below 12.0.0).",
            "PackageName is the constant ferrofin-server; upstream reports its --package-name \
         startup option and omits the member without one. Fix path: gl-1755, whose default \
         awaits an owner decision.",
            "With no server name saved, ServerName falls back to the server_name setting \
         (FERROFIN_SERVER_NAME), whose default is upstream's Environment.MachineName; another \
         value diverges, as upstream has no such setting. Like every bootstrap setting it is \
         resolved when the server starts or restarts (the settings page's Applies: Restart), \
         where upstream reads the machine name on every call, and a host name that leaves no \
         name before its first dot gives ferrofin where upstream answers an empty name.",
            "LocalAddress is chosen for the client address with an IPv4-mapped IPv6 address \
         unmapped, as GetNormalizedRemoteIP gives it; upstream's GetSmartApiUrl passes \
         Connection.RemoteIpAddress as given, so behind a dual-stack listener an IPv4 client \
         in no interface's subnet is matched by address family to an IPv6 interface. \
         Jellyfin bug not ported.",
            "An Id adopted from Jellyfin's device.txt is trimmed; upstream's DeviceId keeps \
         the whitespace Guid.TryParse accepts around the GUID, and clients receive it. \
         Jellyfin bug not ported.",
            "LocalAddress carries the base URL this lifetime serves under; upstream's \
         GetLocalApiUrl reads a saved BaseUrl at once, while its mount keeps the old one \
         (owner decision on gl-555).",
            "A backslash in the base URL is escaped (%5C) in LocalAddress; upstream's \
         UriBuilder.Path turns it into a /, an address its mount does not answer (12.4's \
         discovery Address, probed). Jellyfin bug not ported.",
            "After a save that turns HTTPS on, LocalAddress stays http until the restart; \
         upstream's ListenWithHttps, over a certificate it loaded at startup with HTTPS off, \
         advertises https on a port that listens only after the restart. Jellyfin bug not \
         ported.",
            "A transcode path that cannot be prepared answers 500 whatever the cause; upstream's \
         status follows the exception its ExceptionMiddleware maps (an ArgumentException 400, \
         a DirectoryNotFoundException 404), describing a server misconfiguration as the \
         client's error. Jellyfin bug not ported.",
            "The 401 (no or bad token, a disabled user), 403 (no remote access) and 500 (a \
         transcode path that cannot be prepared) bodies are Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/System/Info/Storage",
        upstream_file: "Jellyfin.Api/Controllers/SystemController.cs",
        upstream_method: "GetSystemStorage",
        ferrofin: "handlers/system.rs get_system_storage (auth.rs RequireAdmin: RequiresElevation, \
               an API key or an administrator, else 403) → ferrofin-core system_manager.rs \
               get_system_storage_info (SystemManager.GetSystemStorageInfo: the program-data, \
               web, image-cache, cache, log and internal-metadata paths and the live transcode \
               path prepared as GetTranscodePath, then each library's locations from \
               apps/ferrofin-server state.rs VirtualFolderStorage, GetVirtualFolders with an \
               empty ItemId left out and the locations in Locations.Order()) → FsStorageProbe \
               probe_all (StorageHelper.GetFreeSpaceOf: resolve_path for ResolvePath, each link \
               component replaced by its final target and a missing component the catch arm, \
               which reports the path as given with -1 and no type or id; mount_table.rs \
               read_mount_table for DriveInfo.GetDrives(), /proc/mounts or, on Windows, \
               GetLogicalDrives' roots with GetDriveTypeW; mount_for_path for bestMatch, the \
               deepest drive holding the resolved path; disk_space on that drive's Name for \
               AvailableFreeSpace and TotalSize, statvfs's f_bavail and f_blocks or \
               GetDiskFreeSpaceExW's caller and total bytes; DeviceId the drive's Name, a mount \
               point or C:\\) → ferrofin-model \
               system_info_dtos.rs SystemStorageDto::from_system_storage_info \
               (FromSystemStorageInfo, without ResolvedPath)",
        date: "2026-11-15",
        divergences: &[
            "A folder no drive holds (no readable mount table, as on macOS; a chroot that hides \
         `/`; a Windows UNC path no drive letter maps) reports its own filesystem's free and \
         used bytes with StorageType Unknown and no DeviceId, where upstream's \
         bestMatch-is-null throw reports -1 and neither.",
            "The drive match is component-wise, so a mount at /mnt/media never holds \
         /mnt/media2, and case-sensitive outside Windows; upstream's StartsWith is a raw prefix \
         that ignores case everywhere.",
            "ResolvePath resolves a relative path from the working directory, and a `..` from \
         the folder resolved so far on Unix (on Windows std::path::absolute folds it first, as \
         Win32 does), where upstream keeps both as written and then no drive root prefixes the \
         path.",
            "A link is resolved with canonicalize, which also resolves links inside its target \
         and fails on a dangling one (the catch arm), where .NET's GetFinalLinkTarget follows \
         only the chain of links and returns a missing last target, whose drive upstream \
         reports.",
            "A library whose ItemId is not a GUID is left out, where upstream's Guid.Parse throws \
         and the request fails.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/System/Info/Public",
        upstream_file: "Jellyfin.Api/Controllers/SystemController.cs",
        upstream_method: "GetPublicSystemInfo",
        ferrofin: "handlers/system.rs get_public_system_info (no authorization, as the action \
               has no [Authorize]: a token, valid or not, changes nothing) → context_from_parts \
               → ferrofin-core system_manager.rs get_public_system_info \
               (SystemManager.GetPublicSystemInfo): the LocalAddress, ServerName, Version, \
               ProductName, Id and StartupWizardCompleted of the get /System/Info row, and \
               OperatingSystem's fixed empty string. Probed on Jellyfin 12.4 as that row was",
        date: "2026-11-15",
        divergences: &[
            "Version is the Jellyfin release Ferrofin emulates (openapi.rs \
         JELLYFIN_API_VERSION, 12.4.0), not Ferrofin's own release version, which upstream's \
         field would carry. Deliberate: clients gate on it (jellyfin-web refuses a server \
         below 10.10.0, the Kotlin SDK flags one below 12.0.0).",
            "With no server name saved, ServerName falls back to the server_name setting \
         (FERROFIN_SERVER_NAME), whose default is upstream's Environment.MachineName; another \
         value diverges, as upstream has no such setting. Like every bootstrap setting it is \
         resolved when the server starts or restarts (the settings page's Applies: Restart), \
         where upstream reads the machine name on every call, and a host name that leaves no \
         name before its first dot gives ferrofin where upstream answers an empty name.",
            "LocalAddress is chosen for the client address with an IPv4-mapped IPv6 address \
         unmapped, as GetNormalizedRemoteIP gives it; upstream's GetSmartApiUrl passes \
         Connection.RemoteIpAddress as given, so behind a dual-stack listener an IPv4 client \
         in no interface's subnet is matched by address family to an IPv6 interface. \
         Jellyfin bug not ported.",
            "An Id adopted from Jellyfin's device.txt is trimmed; upstream's DeviceId keeps \
         the whitespace Guid.TryParse accepts around the GUID, and clients receive it. \
         Jellyfin bug not ported.",
            "LocalAddress carries the base URL this lifetime serves under; upstream's \
         GetLocalApiUrl reads a saved BaseUrl at once, while its mount keeps the old one \
         (owner decision on gl-555).",
            "A backslash in the base URL is escaped (%5C) in LocalAddress; upstream's \
         UriBuilder.Path turns it into a /, an address its mount does not answer (12.4's \
         discovery Address, probed). Jellyfin bug not ported.",
            "After a save that turns HTTPS on, LocalAddress stays http until the restart; \
         upstream's ListenWithHttps, over a certificate it loaded at startup with HTTPS off, \
         advertises https on a port that listens only after the restart. Jellyfin bug not \
         ported.",
        ],
        refs: &[],
    },
    Verified {
        method: "delete",
        path: "/Videos/{itemId}/Subtitles/{index}",
        upstream_file: "Jellyfin.Api/Controllers/SubtitleController.cs",
        upstream_method: "DeleteSubtitle",
        ferrofin: "handlers/subtitles.rs delete_subtitle (RequireAdmin, the RequiresElevation \
               policy; a nil itemId is GetItemById's ArgumentException; require_item, \
               GetItemById<BaseItem>(itemId, User.GetUserId()): a missing or hidden item is \
               NotFound()'s ProblemDetails 404; every other failure answered as \
               ExceptionMiddleware answers a throw, its status with text/plain \"Error \
               processing request.\" (error.rs into_exception_response); the delete and the \
               refresh on handlers.rs run_detached's task, in the request's span and culture, \
               as upstream finishes them for a client that hangs up, logging a failure no \
               answer carries) → ferrofin-core subtitle_manager.rs delete_subtitles \
               (SubtitleManager.DeleteSubtitles: GetMediaStreams for the subtitle stream at \
               index, none or one with no or an empty path the 400; a row not marked \
               external is skipped too, which changes no answer, since neither server stores \
               a path on one, and keeps a stray path from deleting the media file; the file \
               deleted inside the self-write bracket as File.Delete does on Linux: a file \
               already gone ignored while its folder exists, else, and for ENOTDIR (a folder \
               in the path is a file; read in dotnet/runtime's FileSystem.Unix DeleteFile \
               and Interop.GetExceptionForIoErrno, not measured), the \
               DirectoryNotFoundException 404 before any refresh; an http(s) row, which \
               Path.GetFullPath resolves against the working directory (from /, \
               http://h/a.srt → /http:/h/a.srt, a missing folder), is that 404 without \
               touching the disk; on a read-only mount, where unlink answers EROFS before \
               it looks the file up, the file is already gone when an lstat of the path \
               fails with ENOENT, and anything else is the 500 (DeleteFile's EROFS arm over \
               FileSystem.Exists.Unix FileExists, dotnet/runtime issue 22382; read on \
               release/10.0, not measured on 12.4)) → \
               handlers.rs run_item_refresh (the awaited \
               item.RefreshMetadata, whose re-probe drops the stream and renumbers the rest; \
               queued instead for an item outside every library, as for the lyrics delete) → \
               subtitle_manager.rs drop_missing_subtitle_rows (the item's external subtitle \
               rows whose absolute path is gone dropped, the rest kept with their indexes; \
               nothing to drop after a refresh that probed). \
               The parity sweep's request (the corpus movie's stream 0, its video) expects \
               the 400. Measured on 12.4: an embedded, a \
               missing and a video stream's index, a nil id, an unknown id and an external \
               sidecar's index; and that sidecar's row pointed at a missing file (204), a \
               missing folder (404, row kept) and an http URL (404, row kept)",
        date: "2026-11-17",
        divergences: &[
            "A hex (0x, #, &h) or padded index is a 400; upstream's route binder reads it as \
         a number. Fix path: gl-1664.",
            "The 400 for an itemId or index the route binder cannot convert (upstream's \
         ValidationProblemDetails), and the 401 and 403 bodies, are Ferrofin's error shape, \
         not upstream's. Fix path: gl-666.",
            "A row an adopted database stores with a %AppDataPath% or %MetadataPath% token (a \
         subtitle Jellyfin saved in its metadata folder) is read unexpanded, so it is the \
         folder-missing 404 below, where upstream expands it (MediaStreamRepository \
         RestorePath) and deletes the file. Fix path: gl-1063.",
            "Every other relative path is the folder-missing 404, without touching the disk, as \
         the item delete refuses one (item_deletion.rs delete_item_path). Upstream \
         resolves one against its working directory, so a relative row whose folder exists \
         there answers 204 and deletes the file it names beside the server process (gl-1723). \
         A Jellyfin bug, deliberately not ported.",
            "After the refresh, the item's external subtitle rows whose absolute path is gone \
         are dropped, the other rows keeping their indexes; upstream leaves every row to the \
         refresh. The refresh rewrites the rows only when it probes the file, so on a server \
         without ffprobe, or after a failed probe, the deleted subtitle stayed listed \
         (gl-1875); the indexes keep the gap until a refresh that probes renumbers them. \
         After a refresh that probed there is nothing to drop and the rows are upstream's. \
         Upstream keeps the row after a failed probe (read in FFProbeVideoInfo.ProbeVideo, \
         not measured): a Jellyfin bug, deliberately not ported.",
        ],
        refs: &["access-filtering"],
    },
    Verified {
        method: "get",
        path: "/FallbackFont/Fonts",
        upstream_file: "Jellyfin.Api/Controllers/SubtitleController.cs",
        upstream_method: "GetFallbackFontList",
        ferrofin: "handlers/subtitles.rs get_fallback_font_list → ferrofin-core file_system.rs \
               get_files",
        date: "2026-10-30",
        divergences: &[
            "With no FallbackFontPath, upstream also sets EnableFallbackFont to false on the \
         cached EncodingOptions object, unsaved, so that a later read of the configuration \
         reports false until the next restart. Jellyfin bug not ported: a GET does not \
         change the configuration here.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/FallbackFont/Fonts/{name}",
        upstream_file: "Jellyfin.Api/Controllers/SubtitleController.cs",
        upstream_method: "GetFallbackFont",
        ferrofin: "handlers/subtitles.rs get_fallback_font (RequiredRouteString name: \
               ConvertEmptyStringToNull, then [Required]) → ferrofin-core file_system.rs \
               get_files, open_file → handlers/streaming.rs opened_file_response, \
               physical_file_precondition; the type from ferrofin-model mime_types.rs \
               get_mime_type (GetMimeType(fontFile.FullName), compared 2026-11-14)",
        date: "2026-10-30",
        divergences: &[
            "A precondition date outside HTTP's three formats or before 1970 is ignored; \
         ASP.NET's date parser takes both (a numeric offset, for one). No client sends one, \
         short of echoing the Last-Modified of a font dated before 1970, which then gets a \
         200 where upstream sends a 304.",
            "The EnableFallbackFont side effect of a missing FallbackFontPath is not ported, as \
         on GET /FallbackFont/Fonts.",
            "The 404 and 500 of a font that cannot be opened have Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Items/{itemId}/RemoteSearch/Subtitles/{language}",
        upstream_file: "Jellyfin.Api/Controllers/SubtitleController.cs",
        upstream_method: "SearchRemoteSubtitles",
        ferrofin: "handlers/subtitles.rs search_remote_subtitles (RequireSubtitleManagement, the \
               SubtitleManagement policy's UserPermissionRequirement; RequiredRouteString \
               language: ConvertEmptyStringToNull, then [Required]; require_video, \
               GetItemById<Video>(itemId, User.GetUserId()): a missing, hidden or non-video \
               item is a 404; isPerfectMatch) → ferrofin-core subtitle_manager.rs \
               search_subtitles and enrich (SubtitleManager.SearchSubtitles, both overloads: \
               the language's two-letter code from FindLanguageInfo; [] unless the video's \
               VideoType is VideoFile; the request filled from \
               a Movie or an Episode and [] for any other kind; every provider that supports \
               the content type searched, results in provider order, a failing provider \
               contributing none; Normalize's MD5 provider-name prefix on each id; \
               isPerfectMatch's hash-match filter applied to every provider's results, \
               where upstream leaves it to the provider, as the plugin's MatchFilter does) → \
               ferrofin-providers opensubtitles.rs search (the OpenSubtitles plugin's Search, \
               v25 @ 60d2c74d: an account's Login first, sharing the downloads' session, and \
               no login with an account the API refused (CredentialsInvalid: a 401, or a 400 \
               for an e-mail username); an automated search's [] without a login or with the \
               quota spent; the [] for an episode without its numbers or series name and for \
               a video without a path; \
               GetLanguage's zh-CN and pt-PT checked against the /infos/languages list, read \
               once, a regioned code falling back to its language and no search for a language \
               the list or the culture lacks; the movie hash; type, then imdb_id or else the \
               file name as query with an episode's numbers; AddQueryString's sorted, lowercased \
               query; every page of SearchSubtitlesAsync; BadSubtitleFilter, with the files a \
               download found empty, MediaFilter and MatchFilter; the order by hash match, \
               downloads, rating and trusted uploader; one candidate per result, its first \
               file's id, with ProviderName \"Open Subtitles\" (the plugin's Name, also the \
               fetcher name DisabledSubtitleFetchers and SubtitleFetcherOrder hold), Release, \
               Comments, Ratings, UploadDate, Fps and the machine and AI translation flags)",
        date: "2026-11-14",
        divergences: &[
            "With an OpenSubtitles API key and no account, a manual search still searches; \
         the plugin's Login throws without an account, so it finds nothing. Fix path: gl-808 \
         (owner decision).",
            "OpenSubtitles keeps an account the API refused (CredentialsInvalid) only until the \
         server restarts, the account changes or a login with it succeeds (the settings page's \
         validation included), and a download still tries to log in with it; \
         the plugin saves the flag to its configuration until its settings page validates new \
         credentials, and fails such a download at once (Unable to login). Fix path: gl-1644.",
            "The 401 (no or bad token), 403 (no subtitle management) and 404 bodies are \
         Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["access-filtering"],
    },
    Verified {
        method: "post",
        path: "/Items/{itemId}/RemoteSearch/Subtitles/{language}",
        upstream_file: "Jellyfin.Api/Controllers/SubtitleController.cs",
        upstream_method: "DownloadRemoteSubtitles",
        ferrofin: "handlers/subtitles.rs download_remote_subtitles (RequireSubtitleManagement, the \
               SubtitleManagement policy's UserPermissionRequirement; require_video, \
               GetItemById<Video>(itemId, User.GetUserId()): a missing, hidden or non-video \
               item is a 404; the download and the refresh queued after it share one try, \
               whose failure is logged at error as \"Error downloading subtitles\" and still \
               answered 204; the try runs on handlers.rs run_detached's task, so for a client \
               that hangs up both still finish and a failure is still logged, as \
               DownloadSubtitles gets CancellationToken.None) → ferrofin-core \
               subtitle_manager.rs download_subtitles, route and attach \
               (SubtitleManager.DownloadSubtitles, GetProvider and GetRemoteSubtitles' \
               Split('_', 2): an id naming no provider fails the download; TrySaveSubtitle and \
               TrySaveToFiles: the library's SaveSubtitlesWithMedia picks \
               Video.ContainingFolderPath (LibraryManager containing_folder_path: a DVD or \
               Blu-ray folder rip's own folder, else the media file's) or the item's \
               internal-metadata folder, \
               {stem}.{language}[.forced][.sdh].{format} with the language lowercased and a \
               path separator refused, the format checked against \
               NamingOptions.SubtitleFileExtensions, a taken name moved to {name}.{n}.{format}, \
               the name written bracketed as a self-write; a failed fetch or save, but a \
               RateLimitExceeded, raises SubtitleDownloadFailure, which \
               SubtitleDownloadFailureLogger records: SubtitleDownloadFailureFromForItem with \
               the provider's name and GetItemName, type SubtitleDownloadFailure, no user, the \
               item id, the error message as ShortOverview) → handlers.rs \
               queue_high_priority_refresh (QueueRefresh at RefreshPriority.High) → \
               ferrofin-providers opensubtitles.rs get_subtitles (the OpenSubtitles plugin's \
               GetSubtitles: a plugin outside the Jellyfin tree, not compared yet, gl-1644)",
        date: "2026-11-14",
        divergences: &[
            "The failed download's activity entry is written in English, not the server's UI \
         culture. Fix path: gl-429.",
            "The entry names the OpenSubtitles provider opensubtitles; the plugin's Name, which \
         upstream's entry shows, is Open Subtitles. Fix path: gl-1704.",
            "The subtitle is recorded as an external stream as soon as it is written; upstream \
         leaves that to the queued refresh's probe, so the item lists it here before the \
         refresh has run, and also when queueing the refresh failed. Fix path: gl-1641.",
            "A nil itemId is a 404, not GetItemById's 400. Fix path: gl-1695.",
            "A whitespace-only subtitleId is taken (a logged failed download and a 204, or a \
         404 for a missing or hidden item); upstream's model validation answers 400 before \
         the action runs. Fix path: gl-1061.",
            "The 400 (a malformed itemId), 401 (no or bad token), 403 (no subtitle \
         management) and 404 bodies are Ferrofin's error shape, not upstream's. Fix path: \
         gl-666.",
        ],
        refs: &["access-filtering"],
    },
    Verified {
        method: "get",
        path: "/Providers/Subtitles/Subtitles/{subtitleId}",
        upstream_file: "Jellyfin.Api/Controllers/SubtitleController.cs",
        upstream_method: "GetRemoteSubtitles",
        ferrofin: "handlers/subtitles.rs get_remote_subtitles (RequireSubtitleManagement, the \
               SubtitleManagement policy's UserPermissionRequirement; RequiredRouteString \
               subtitleId: ConvertEmptyStringToNull, then [Required]) → ferrofin-core \
               subtitle_manager.rs get_remote_subtitles and route (SubtitleManager.\
               GetRemoteSubtitles and GetProvider: Split('_', 2), the first provider whose MD5 \
               name id equals the prefix, ordinally, handed the rest of the id, or the whole id \
               when it holds no _) → ferrofin-providers opensubtitles.rs get_subtitles (the \
               OpenSubtitles plugin's GetSubtitles: a plugin outside the Jellyfin tree, not \
               compared yet, gl-1644) → subtitle_mime (File(result.Stream, \
               MimeTypes.GetMimeType(\"file.\" + result.Format)): ferrofin-model \
               mime_types::get_mime_type, application/octet-stream for a format the table \
               does not name; the body whole, no Range, no Content-Disposition)",
        date: "2026-11-16",
        divergences: &[
            "An id naming no provider is a 400; upstream's GetProvider (First) throws \
         InvalidOperationException, a 500. Jellyfin bug not ported.",
            "The 400, 401 (no or bad token) and 403 (no subtitle management) bodies are \
         Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Items/{itemId}/File",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetFile",
        ferrofin: "handlers/library.rs get_file (RequireAuth, the default policy; \
               get_item_by_id_for_user, GetItemById<BaseItem>(itemId, User.GetUserId()): a \
               missing item or one the user cannot see is a 404, an API key sees any item; an \
               item with no path is a 400, MimeTypes.GetMimeType's ArgumentException) → \
               handlers/streaming.rs physical_file_path (IsFileProtocol, then \
               FileSystemHelper.ResolveLinkTarget(filePath, returnFinalTarget: true): a link \
               whose final target exists is served as that target, any other path as given; \
               a directory or a link to one is PhysicalFile's \
               FileNotFoundException, a 404) → serve_physical_file (PhysicalFile(filePath, \
               MimeTypes.GetMimeType(filePath), true): serve_static_file, a missing file a 404, \
               typed by ferrofin-model mime_types::get_mime_type)",
        date: "2026-11-16",
        divergences: &[
            "A nil itemId is a 404, not GetItemById's 400. Fix path: gl-1695.",
            STATIC_FILE_CONDITIONAL_DIVERGENCE,
            "The 400 (no path), 401 (no or bad token), 403 (remote access or the access \
         schedule refused) and 404 bodies are Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &["access-filtering", "static-file-ranges"],
    },
    Verified {
        method: "get",
        path: "/Items/{itemId}/Download",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetDownload",
        ferrofin: "handlers/videos.rs get_download (RequireDownload, the Download policy's \
               UserPermissionRequirement: an API key passes; get_item_by_id_for_user, \
               GetItemById<BaseItem>(itemId, user): a missing item or one the user cannot see \
               is a 404; CanDownload(user) for a user, CanDownload() for an API key: \
               EnableContentDownloading, then ferrofin-core dto_service.rs can_download_file, \
               a 400 when either fails; an item with no path is a 400, \
               MimeTypes.GetMimeType's ArgumentException; attachment_disposition over \
               Path.GetFileName(item.Path) with every \" removed, SetHttpFileName's two forms \
               (filename= sanitized per UTF-16 code unit, filename* RFC 5987 encoded), \
               on a 200, 206 or 416 only, as SetHeadersAndLog sets it past its 304/412 short \
               cut and a missing file's 404 never reaches it) → handlers/streaming.rs \
               physical_file_path (IsFileProtocol, then \
               FileSystemHelper.ResolveLinkTarget(filePath, returnFinalTarget: true): a link \
               whose final target exists is served as that target, any other path as given; \
               a directory or a link to one is PhysicalFile's \
               FileNotFoundException, a 404) → serve_physical_file (PhysicalFile(filePath, \
               MimeTypes.GetMimeType(filePath), filename, true): serve_static_file, a missing \
               file a 404, typed by ferrofin-model mime_types::get_mime_type)",
        date: "2026-11-16",
        divergences: &[
            "LogDownloadAsync is not ported: a user's download writes no \
         UserDownloadingContent activity entry. Fix path: gl-430.",
            "A nil itemId is a 404, not GetItemById's 400. Fix path: gl-1695.",
            STATIC_FILE_CONDITIONAL_DIVERGENCE,
            "The 400 (an item that cannot be downloaded, or no path), 401 (no or bad token), \
         403 (no download permission) and 404 bodies are Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &["access-filtering", "static-file-ranges"],
    },
    Verified {
        method: "get",
        path: "/Videos/{itemId}/{container}/Attachments/{index}",
        upstream_file: "Jellyfin.Api/Controllers/VideoAttachmentsController.cs",
        upstream_method: "GetAttachment",
        ferrofin: "handlers/hls.rs get_video_attachment (RequiredRouteString mediaSourceId, \
               registered as {container}: ConvertEmptyStringToNull, then [Required], before \
               the item is read) → ferrofin-mediaencoding \
               attachments/extractor.rs get_attachment, run_to_completion (the extraction \
               on a task of its own: upstream passes CancellationToken.None), \
               extract_attachment (AttachmentExtractor.GetAttachment, GetAttachmentStream, \
               ExtractAttachment, ExtractAttachmentInternal) → apps/ferrofin-server \
               media_encoding.rs MediaSourceManagerResolver, FfmpegAttachmentIo open_file, \
               run_ffmpeg → handlers/streaming.rs opened_file_response (FileStreamResult)",
        date: "2026-11-11",
        divergences: &[
            "The media source comes from GetStaticMediaSources: upstream's \
         GetPlaybackMediaSources(item, null, true, false) first re-probes a .strm item (and \
         one with no main stream), so a .strm whose target changed serves its last probe's \
         attachments here. Fix path: gl-1519. Its dynamic sources, a channel's tuner \
         streams, are in the static list here, and list no attachments on either side (a \
         404).",
            "A nil videoId is a 404, not GetItemById's 400. Fix path: gl-1695.",
            "A mediaSourceId selects its source in any GUID spelling; upstream compares the N \
         form, ignoring case. Clients echo the id they were given, and an adopted database \
         may hold the hyphenated form.",
            "An attachment MIME type that MediaTypeHeaderValue.Parse refuses (FileStreamResult \
         parses it) is a 500 upstream. Here it is sent as stored, or as \
         application/octet-stream, as a missing one is, when no header can carry it (a \
         control or non-ASCII character). Jellyfin bug not ported.",
            "An item that cannot have media sources (a series, a box set) is a 404 (no \
         attachment with that index); upstream's GetStaticMediaSources throws \
         ArgumentException, a 400. Fix path: gl-397.",
            "The 400, 404 and 500 bodies have Ferrofin's error shape, not upstream's: a missing \
         item is NotFound()'s problem details there, a cached file gone before it is opened \
         the exception middleware's text. Fix path: gl-666.",
        ],
        refs: &["access-filtering"],
    },
    Verified {
        method: "delete",
        path: "/Videos/ActiveEncodings",
        upstream_file: "Jellyfin.Api/Controllers/HlsSegmentController.cs",
        upstream_method: "StopEncodingProcess",
        ferrofin: "handlers/hls.rs stop_encoding_process (RequireAuth: [Authorize]; Query: \
               deviceId and playSessionId from query/required.json) → ferrofin-hls \
               hls_stream_manager.rs stop_encoding → ferrofin-mediaencoding \
               transcoding/manager.rs stop_transcoding_jobs and kill_matching \
               (TranscodeManager.KillTranscodingJobs, not awaited: the jobs of that \
               playSessionId, ignoring ASCII case, each stopped before the 204, as \
               KillTranscodingJob runs up to its first await), remove_and_tear_down and \
               tear_down (KillTranscodingJob: out of the registry, \
               TranscodingJob.Stop, the live stream left open), then on a task of its own \
               delete_partial_stream_files and FsFileCleaner (DeletePartialStreamFiles and \
               DeleteHlsPartialStreamFiles: 1500 ms before the first of at most ten attempts, \
               500 ms before each retry, which only an IOException earns); here, unlike \
               upstream, an attempt is skipped once a registered job has the output path",
        date: "2026-11-17",
        divergences: &[
            "A job started on the stopped job's output path while the deletion waits (the same \
         stream asked for again) keeps its files: the deletion ends at the first attempt that \
         finds it registered, and the job's own deletion takes them when it is stopped, else \
         the transcode-directory cleanup. Upstream deletes them, the new job's playlist and \
         segments with them. Jellyfin bug not ported (gl-1876).",
            "A killed progressive job's files go by the HLS rule, every file in its directory \
         whose name holds its stem, so its trimmed subtitle copy goes with its output; \
         upstream deletes the output file alone and makes no such copy.",
            "Access denied on one file does not end the attempt, so the job's other files are \
         still deleted; upstream's UnauthorizedAccessException leaves its loop at that file. \
         A directory the server may not list is logged as an error; upstream's \
         IgnoreInaccessible lists it as empty and logs nothing. A file is the job's when its \
         name holds the stem; upstream matches the whole path, so a directory whose own path \
         holds the stem would give up every file in it. Jellyfin bugs not ported.",
            "A file left behind is logged at debug, and each failed attempt once with the count \
         left, a warning while it will be retried and an error at the last; upstream logs \
         each file and each attempt at Error (LOGGING.md: nothing above debug that grows \
         with the segments, an error logged once).",
            "The 400 for a missing, empty or whitespace-only deviceId or playSessionId is \
         Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Items/Root",
        upstream_file: "Jellyfin.Api/Controllers/UserLibraryController.cs",
        upstream_method: "GetRootFolder",
        ferrofin: "handlers/user_library.rs get_root_folder (resolve_user: \
               RequestHelpers.GetUserId, then GetUserById) → ferrofin-core library_manager.rs \
               get_user_root_folder → user_root_folder.rs UserRootFolderStore::ensure, once \
               per process (LibraryManager.GetUserRootFolder: the directory created, the row \
               read by its derived id and created when missing, its Path reset when it \
               differs ordinally, as after a program data move) → dto_service.rs \
               get_base_item_dto with DtoOptions::default() (new DtoOptions())",
        date: "2026-11-15",
        divergences: &[
            "A created or reset row is saved at once; upstream keeps it in memory until the \
         root's next saving refresh (LibraryManager.cs:1449-1452). Both answer the same row.",
            "A missing row is created already named Media Folders; upstream's is named after \
         its directory (default) until its first refresh renames it \
         (UserRootFolder.BeforeMetadataRefresh), so only a new database's root differs.",
            "The 400 (an API key with no userId, or a value the query binder cannot convert), \
         403 (another user's id without the administrator role) and 404 (a userId naming no \
         account) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["images", "user-data"],
    },
    Verified {
        method: "get",
        path: "/Items/Latest",
        upstream_file: "Jellyfin.Api/Controllers/UserLibraryController.cs",
        upstream_method: "GetLatestMedia",
        ferrofin: "handlers/user_library.rs get_latest_media → ferrofin-core \
               user_view_manager.rs get_latest_items, items_for_latest_items, latest_parents \
               (UserViewManager.GetItemsForLatestItems, LibraryManager.GetLatestItemList, \
               SetTopParentIdsOrAncestors) → item_repository.rs get_latest_item_list, \
               latest_movie_items, latest_tv_repository.rs \
               (BaseItemRepository.GetLatestItemList, GetLatestMovieItems, GetLatestTvShowItems)",
        date: "2026-11-03",
        divergences: &[
            "The plain query over-fetches 10.11.8's limit * 5 where v12 fetches limit * 2, so \
         a request answered by it can reach containers upstream's cut leaves out. Fix path: \
         gl-303.",
            "A Channel parent answers empty: upstream's channel manager reads plugin channels, \
         and Ferrofin has none (native .NET plugin loading is out of scope).",
            "The 400 (an API key with no userId, or a value the query binder cannot convert), \
         403 (another user's id without the administrator role) and 404 (a userId naming no \
         account) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["images", "access-filtering"],
    },
    Verified {
        method: "get",
        path: "/UserViews/GroupingOptions",
        upstream_file: "Jellyfin.Api/Controllers/UserViewsController.cs",
        upstream_method: "GetGroupingOptions",
        ferrofin: "handlers/user_views.rs get_grouping_options → ferrofin-core \
               user_view_manager.rs get_user_root_children",
        date: "2026-10-31",
        divergences: &[
            "The 400 (an API key with no userId, or a value the query binder cannot convert), \
         403 (another user's id without the administrator role) and 404 (a userId naming no \
         account) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Devices/Options",
        upstream_file: "Jellyfin.Api/Controllers/DevicesController.cs",
        upstream_method: "UpdateDeviceOptions",
        ferrofin: "handlers/devices.rs update_device_options (RequireAdmin, \
               Policies.RequiresElevation; a missing or blank id and an empty body are 400) → \
               ferrofin-core device_manager.rs update_device_options (DeviceManager.\
               UpdateDeviceOptions: one upsert of CustomName, null when the body has none) → \
               session_manager.rs on_device_options_updated (the DeviceOptionsUpdated handler \
               SessionManager.OnDeviceManagerDeviceOptionsUpdated: every live session of the \
               device takes a non-blank name as its DeviceName with HasCustomDeviceName, a \
               blank one only clears HasCustomDeviceName)",
        date: "2026-11-10",
        divergences: &[
            "The 400 for a missing or blank id, the 401 (no or bad token) and the 403 (not an \
         administrator) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/ScheduledTasks",
        upstream_file: "Jellyfin.Api/Controllers/ScheduledTasksController.cs",
        upstream_method: "GetTasks",
        ferrofin: concat!(
            "handlers/scheduled_tasks.rs get_tasks (RequireAdmin, Policies.RequiresElevation; \
             isHidden and isEnabled as bool?) → ferrofin-core scheduled_tasks.rs list_filtered \
             (ScheduledTasks.OrderBy(o => o.Name) in the current culture; the two filters only \
             on an IConfigurableScheduledTask) → ",
            scheduled_task_info!()
        ),
        date: "2026-11-15",
        divergences: &[
            ADOPTED_TRIGGER_FILE_DIVERGENCE,
            ADOPTED_FILE_LENIENCY_DIVERGENCE,
            "The 400 (an isHidden or isEnabled value the query binder cannot convert), 401 (no \
         or bad token) and 403 (not an administrator) bodies are Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/ScheduledTasks/{taskId}",
        upstream_file: "Jellyfin.Api/Controllers/ScheduledTasksController.cs",
        upstream_method: "GetTask",
        ferrofin: concat!(
            "handlers/scheduled_tasks.rs get_task (RequireAdmin, Policies.RequiresElevation; \
             RequiredRouteString taskId: ConvertEmptyStringToNull, then [Required]) → \
             ferrofin-core scheduled_tasks.rs get_task (the Id matched ignoring case, as \
             OrdinalIgnoreCase; none is the 404) → ",
            scheduled_task_info!()
        ),
        date: "2026-11-15",
        divergences: &[
            ADOPTED_TRIGGER_FILE_DIVERGENCE,
            ADOPTED_FILE_LENIENCY_DIVERGENCE,
            "The 401 (no or bad token), 403 (not an administrator) and 404 (no task has that \
         id) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "delete",
        path: "/ScheduledTasks/Running/{taskId}",
        upstream_file: "Jellyfin.Api/Controllers/ScheduledTasksController.cs",
        upstream_method: "StopTask",
        ferrofin: "handlers/scheduled_tasks.rs stop_task (RequireAdmin, \
               Policies.RequiresElevation; RequiredRouteString taskId: ConvertEmptyStringToNull, \
               then [Required]) → ferrofin-core scheduled_tasks.rs cancel_task (the Id matched \
               ignoring case, as OrdinalIgnoreCase; none is the 404) → cancel → \
               Registration::request_stop (ScheduledTaskWorker.Cancel/CancelIfRunning: the \
               run's token cancelled, State Cancelling until the run returns) → \
               execute_contained and finish (ExecuteInternal: the run records Cancelled when \
               its body stopped for it, else its own outcome, then ExecuteQueuedTasks)",
        date: "2026-11-11",
        divergences: &[
            "Stopping a task that is idle or already stopping answers 204 and changes nothing; \
         upstream's Cancel throws InvalidOperationException there, a 500. Jellyfin bug not \
         ported.",
            "A body still running STOP_WAIT (2 s) after the stop is dropped and recorded \
         Cancelled; upstream waits for ExecuteAsync however long it takes. Fix path: every \
         task body checks its token as upstream's do (gl-1709); the value is a candidate \
         setting (gl-1710).",
            "The 401 (no or bad token), 403 (not an administrator) and 404 (no task has that \
         id) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Library/Refresh",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "RefreshLibrary",
        ferrofin: "handlers/library.rs refresh_library (RequireAdmin, \
               Policies.RequiresElevation) → TaskManager::cancel_if_running_and_queue on the \
               RefreshLibrary task's wire id (LibraryManager.ValidateMediaLibrary → \
               TaskManager.CancelIfRunningAndQueue<RefreshMediaLibraryTask>) → ferrofin-core \
               scheduled_tasks.rs cancel (CancelIfRunning) and queue_scheduled \
               (QueueScheduledTask with new TaskOptions(): started when Idle, else queued \
               until the cancelled run has ended); a failure is logged as \"Error refreshing \
               library\" and the answer is 204 either way, as the controller's catch",
        date: "2026-11-11",
        divergences: &[
            "A Scan Media Library run the refresh cancels ends when its scan wait is \
         withdrawn, before the scan's item in progress does; upstream's run stays Cancelling \
         until ValidateMediaLibraryInternal returns. The scan queue still starts the new \
         scan after the stopping one. Fix path: gl-1709.",
            "The 401 (no or bad token) and 403 (not an administrator) bodies are Ferrofin's \
         error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/Apply/{itemId}",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "ApplySearchCriteria",
        ferrofin: "handlers/item_lookup.rs apply_search_criteria (RequireAdminWithDefault: \
               Policies.RequiresElevation under the controller's default policy; the item by \
               GetItemById(itemId, User.GetUserId()), a 404 when it is missing or the caller \
               cannot see it; replaceAllImages defaults to true; the body is the chosen \
               RemoteSearchResult, logged with its provider ids; GetItemById's 400 for a nil \
               id) → MetadataRefreshOptions \
               with FullRefresh for metadata and images, ReplaceAllMetadata, \
               RemoveOldMetadata, ReplaceAllImages = replaceAllImages and SearchResult = the \
               body → LibraryManager::refresh_full_item (RefreshFullItem) awaited on \
               handlers.rs run_detached's task (CancellationToken.None; the request's span \
               and culture, a failure after a hang-up logged), routed by ferrofin-core \
               library_manager/item_refresh.rs identify_target: an item with a path \
               through run_refresh_scan of scan_target in the scan queue's priority lane \
               (RefreshItem: the item, then ValidateChildren with the same options, so a \
               folder's children refresh under the chosen result too), an artist known only \
               by name through the scanner's ScanTarget::Artist, any other item through \
               ProviderManager::refresh_full_item; the chosen ids replace the item's \
               (item.SetProviderIds) with the refresh's save of it, a path-less item's \
               before its providers run → 204. A refresh stopped by the server shutting \
               down answers 503.",
        date: "2026-11-14",
        divergences: &[
            "An item whose path no library location covers, or a file-backed item in no \
         library, answers 409 and is not refreshed; upstream refreshes it. Fix path: \
         gl-1770.",
            "The 400 (an itemId, replaceAllImages or body the binder cannot read, or the nil \
         id), 401 (no or bad token), 403 (not an administrator, or a remote caller the \
         user's remote-access restriction refuses) and 404 bodies are Ferrofin's error \
         shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[
            "metadata-refresh",
            "metadata-providers",
            "images",
            "access-filtering",
            "scan-failure-isolation",
            "trickplay",
        ],
    },
    Verified {
        method: "post",
        path: "/Items/{itemId}/Refresh",
        upstream_file: "Jellyfin.Api/Controllers/ItemRefreshController.cs",
        upstream_method: "RefreshItem",
        ferrofin: "handlers/item_update.rs refresh_item (RequireAdmin, \
               Policies.RequiresElevation; the item by GetItemById(itemId, \
               User.GetUserId()), a 404 when it is missing or the caller cannot see it and \
               a 400 for a nil id; metadataRefreshMode and imageRefreshMode default to None \
               and replaceAllMetadata, replaceAllImages and regenerateTrickplay to false; \
               MetadataRefreshOptions::for_item_refresh: ForceSave for a FullRefresh of \
               either kind or either replace, RemoveOldMetadata = replaceAllMetadata, \
               IsAutomated false, RegenerateTrickplay) → \
               LibraryManager::queue_item_refresh, routed by ferrofin-core \
               library_manager/item_refresh.rs refresh_target (QueueRefresh at High \
               priority, then the queue's RefreshItem or RefreshArtist): a \
               CollectionFolder's physical folders \
               (RefreshCollectionFolderChildren), every library for the root, another \
               folder's subtree (the folder, then ValidateChildren with the same options) \
               and a file's own path (RefreshSingleItem's metadata service), each as a scan \
               through queue_refresh_scan; a music artist by artist_target \
               (RefreshArtist: the artist folders above its credited albums, found from \
               one read of the ancestors per album folder as MusicAlbum.GetMusicArtist \
               walks them, validate their children, then the artist refreshes); a box set, \
               playlist, view, virtual season, person, genre, studio or channel through \
               ProviderManager::queue_refresh at High priority → 204 without waiting for \
               the refresh.",
        date: "2026-11-14",
        divergences: &[
            "An item whose path no library location covers is answered 204 and not \
         refreshed; upstream refreshes it. Fix path: gl-1770.",
            "Upstream runs every queued item refresh on one queue, started at once and one \
         after another; Ferrofin runs a file's refresh in the scan queue's priority lane \
         (ahead of queued scans, inside a running one), queues a folder's like a scan, \
         behind a running or queued library scan, and an item no scan covers (a box \
         set, playlist, view, virtual season, person, genre, studio or channel) on the \
         provider manager's queue, so refreshes on different lanes can run at once or in \
         another order. Fix path: gl-1814.",
            "The 400 (a value the binder cannot read, or the nil id), 401 (no or bad \
         token), 403 (not an administrator) and 404 bodies are Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[
            "metadata-refresh",
            "metadata-providers",
            "images",
            "access-filtering",
            "scan-failure-isolation",
            "trickplay",
        ],
    },
    Verified {
        method: "get",
        path: "/Library/MediaFolders",
        upstream_file: "Jellyfin.Api/Controllers/LibraryController.cs",
        upstream_method: "GetMediaFolders",
        ferrofin: "handlers/library.rs get_media_folders (RequireAdmin, \
               Policies.RequiresElevation; no user read, so an API key is served; isHidden \
               keeps the folders whose IsHidden equals it, true only for PlaylistsFolder \
               under either stored spelling) → ferrofin-core user_view_manager.rs \
               get_media_folders (root_children for GetUserRootFolder().Children and \
               RootFolder.VirtualChildren, the Live TV view left out; item_visibility.rs \
               without_disabled_libraries for GetLibraryOptions(i).Enabled, which the \
               playlists folder always passes; OrderBy(i => i.SortName) as a stable sort \
               of the libraries then the playlists folder by item_repository.rs \
               folder_sort_name under the request culture's comparer) → \
               DtoService get_base_item_dtos with DtoOptions::default() and no user (new \
               DtoOptions(), no UserData) → QueryResult::from_items",
        date: "2026-11-13",
        divergences: &[
            "The 400 (an isHidden value the query binder cannot convert), 401 and 403 bodies \
         are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &["images"],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/MusicAlbum",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetMusicAlbumRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs music_album_remote_search (RequireAuth; JsonBody for \
               the [Required] body; to_request: AlbumInfo's AlbumArtists, ArtistProviderIds and \
               SongInfos ride along) → ferrofin-providers provider_manager.rs remote_search \
               (ProviderManager.GetRemoteSearchResults: the reference item's library options, \
               or none for no ItemId; CanRefreshMetadata: IncludeDisabledProviders first, then \
               a locked item or an extra (ExtraType set) refuses every provider, then the \
               fetcher checkboxes; SearchProviderName ignoring case; the configured then \
               default order; the server's language and country for blank ones; each \
               provider's results stamped with its name and merged by a shared provider id, \
               ImageUrl filled; a failing provider logged and skipped. The providers run \
               together and merge in provider order, which gives the C# loop's list) → \
               MusicBrainzAlbumSearchProvider \
               (MusicBrainzAlbumProvider.GetSearchResults: album_id_or_song_id and \
               album_artist_mbid are AlbumInfoExtensions' GetReleaseId, GetReleaseGroupId and \
               GetMusicBrainzArtistId, each id through IsValidProviderId; or_failed fails the \
               provider on any failed request, as the C# HttpError does, except a by-id \
               lookup's 404 (NotFoundAsNullAsync), so a 404 or malformed release id falls to \
               the release group and an unknown group to the arid:/artist: search; \
               GetReleaseGroupResultAsync's \
               walk in musicbrainz.rs release_group_releases skips a release that answers 404 \
               and stops at one that fails otherwise, keeping the releases before it, as \
               GetRemoteSearchResults keeps what it added before the exception) and \
               AudioDbSearchProvider (AudioDbAlbumProvider: no results)",
        date: "2026-11-14",
        divergences: &[
            "The search by album and artist name drops the stray `c` the C# appends after the \
         artist phrase (`artist:\"…\"c`), a typo that adds a bare term to the lucene query.",
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) body is Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/MusicArtist",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetMusicArtistRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs music_artist_remote_search (RequireAuth; JsonBody for \
               the [Required] body; to_request: ArtistInfo's SongInfos ride along) → \
               ferrofin-providers provider_manager.rs remote_search (as for MusicAlbum: \
               ProviderManager.GetRemoteSearchResults) → MusicBrainzArtistSearchProvider \
               (MusicBrainzArtistProvider.GetSearchResults, Order 0: artist_mbid is \
               AlbumInfoExtensions.GetMusicBrainzArtistId(ArtistInfo) through \
               IsValidProviderId; or_failed fails the provider on any failed request, as the \
               C# HttpError does, except the lookup's 404 (NotFoundAsNullAsync), so a 404 or \
               malformed id falls to the name search; a blank name finds nothing; the phrase \
               search, then artistaccent: for a name with diacritics) and \
               AudioDbSearchProvider",
        date: "2026-11-11",
        divergences: &[
            "TheAudioDB contributes no artists: v12.4's AudioDbArtistProvider.GetSearchResults \
         looks the artist up by its AudioDbArtist id, else its MusicBrainz id, else searches \
         by name. Fix path: gl-497.",
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) body is Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/Movie",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetMovieRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs movie_remote_search (RequireAuth, the controller's \
               [Authorize]; JsonBody for the [Required] body, MovieInfo adding nothing to \
               ItemLookupInfo; to_request) → ferrofin-providers provider_manager.rs \
               remote_search (as for MusicAlbum: ProviderManager.GetRemoteSearchResults) → \
               TmdbSearchProvider for TmdbKind::Movie (TmdbMovieProvider.GetSearchResults, \
               Order 1: a Tmdb id pins the title through tmdb.rs details_for_locale \
               (GetMovieAsync: Title ?? OriginalTitle, the poster, the overview, PremiereDate \
               and ProductionYear from the release date, the Tmdb id and the Imdb id TMDb \
               knows), a failed lookup going on; else an Imdb, then a Tvdb id through \
               find_by_external_id (FindByExternalIdAsync with GetImageLanguagesParam, which it \
               passes through NormalizeLanguage once more), whose \
               movie_results answer even when empty, a failed request going on; else the name \
               search with Year (SearchMovieAsync, include_adult from the plugin settings), \
               each row with its Tmdb id only) and OmdbSearchProvider for OmdbKind::Movie \
               (OmdbItemProvider.GetSearchResults, Order 2: a non-blank Imdb id asks i= for \
               the one record; else the name through ParseName, whose year fills a missing \
               Year, asks s= with type=movie; each row's Title, Imdb id, ProductionYear from \
               Year's first four characters, PremiereDate from Released, the poster unless \
               N/A, and the lookup's IndexNumber and ParentIndexNumber)",
        date: "2026-11-15",
        divergences: &[
            "A TMDb id with surrounding whitespace or a leading sign (` 603 `, `+603`) pins \
         the title, where v12.4's TryParseTmdbId (NumberStyles.None) reads it as no id and \
         goes on to the IMDb and TVDB ids and the name search; and a padded IMDb or TVDB id \
         is trimmed before TMDb's /find and OMDb's i=, where v12.4 sends it as typed. Fix \
         path: gl-1774.",
            "A TMDb result's PremiereDate is the release date at UTC midnight; upstream's \
         ToUniversalTime reads it as the server's local midnight, so on a host not on UTC \
         the instant differs by the offset, and on one east of UTC a 1 January release's \
         ProductionYear is the year before. Fix path: gl-490.",
            "TheTVDB's movie search (jellyfin-plugin-tvdb TvdbMovieProvider.GetSearchResults) \
         is not ported, so a movie Identify lists no TheTVDB candidates where Jellyfin with the \
         plugin does. Fix path: gl-1405.",
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) body is Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/Trailer",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetTrailerRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs trailer_remote_search (RequireAuth, the controller's \
               [Authorize]; JsonBody for the [Required] body, TrailerInfo adding nothing to \
               ItemLookupInfo; to_request) → ferrofin-providers provider_manager.rs \
               remote_search (as for MusicAlbum: ProviderManager.GetRemoteSearchResults) → \
               OmdbSearchProvider::for_trailers, the one trailer provider (OmdbItemProvider's \
               IRemoteMetadataProvider<Trailer, TrailerInfo>, Order 2: GetSearchResultsInternal's \
               `_ => \"movie\"` arm, so type=movie, the search otherwise as Movie's OMDb one)",
        date: "2026-11-14",
        divergences: &[
            "A padded IMDb id is trimmed before OMDb's i=; v12.4 sends it as typed. Fix \
         path: gl-1774.",
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) body is Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/MusicVideo",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetMusicVideoRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs music_video_remote_search (RequireAuth, the \
               controller's [Authorize]; JsonBody for the [Required] body; to_request carries \
               MusicVideoInfo's Artists) → ferrofin-providers provider_manager.rs \
               remote_search (as for MusicAlbum: ProviderManager.GetRemoteSearchResults), where \
               no registered provider supports MusicVideo, so the answer is []: v12.4 has no \
               IRemoteSearchProvider<MusicVideoInfo>",
        date: "2026-11-14",
        divergences: &[
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) body is Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/Series",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetSeriesRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs series_remote_search (RequireAuth, the controller's \
               [Authorize]; JsonBody for the [Required] body, SeriesInfo adding nothing to \
               ItemLookupInfo; to_request) → ferrofin-providers provider_manager.rs \
               remote_search (as for MusicAlbum: ProviderManager.GetRemoteSearchResults) → \
               TmdbSearchProvider for TmdbKind::Series (TmdbSeriesProvider.GetSearchResults, \
               Order 1: a Tmdb id pins the series through tmdb.rs details_for_locale \
               (GetSeriesAsync, MapTvShowToRemoteSearchResult: Name ?? OriginalName, the \
               poster, the overview, the Tmdb id and the Imdb and Tvdb ids from external_ids, \
               PremiereDate and ProductionYear from the first air date), a failed lookup going \
               on; else an Imdb, then a Tvdb id through find_by_external_id with the bare \
               normalized language, whose tv_results answer even when empty, each row stamped \
               with the id searched by; else the name search with no year, SearchSeriesAsync's \
               year left at 0), OmdbSearchProvider for OmdbKind::Series (as for Movie, \
               type=series) and the compiled-in TvdbSearchProvider (jellyfin-plugin-tvdb \
               5c4592f TvdbSeriesProvider.GetSearchResults, no IHasOrder so 50: a Tvdb id, \
               else an Imdb, Zap2It or Tmdb id resolved through /search/remoteid, pins the \
               series' extended record; else tvdb.rs search, the parsed comparable name, five \
               results ranked as FindSeriesInternal ranks them and kept within a year of Year)",
        date: "2026-11-15",
        divergences: &[
            "TheTVDB's results are listed too (SearchProviderName TheTVDB, ProviderIds.Tvdb): \
         Ferrofin compiles its remote providers in (CLAUDE.md, Current scope), where stock \
         v12.4 lists TheTVDB only with jellyfin-plugin-tvdb installed.",
            "TheTVDB's search differs from the plugin's: a lookup whose only id is Tmdb is \
         resolved by that id instead of searched by name, a non-numeric or zero Tvdb id is \
         passed over where the plugin's search fails, a remote id pins the first series among \
         its hits where the plugin reads only the first hit, the pinned result has the \
         record's own-language name and overview, no PremiereDate, the first poster artwork \
         and Tmdb and Zap2It ids, and name-search hits carry an overview but no Imdb, Zap2It \
         or Tmdb id, so none merges into TheMovieDb's row for the same show. Fix path: \
         gl-1775.",
            "A TMDb id with surrounding whitespace or a leading sign (` 1396 `, `+1396`) pins \
         the series, where v12.4's TryParseTmdbId (NumberStyles.None) reads it as no id and \
         goes on to the IMDb and TVDB ids and the name search. A padded IMDb or TVDB id is \
         trimmed before TMDb's /find, an IMDb one before OMDb's i=, and an IMDb, Zap2It or \
         TMDb one before TheTVDB's remote-id search, where v12.4 and the plugin send them as \
         typed; the /find rows are stamped with the id untrimmed, where SetProviderId trims. \
         Fix path: gl-1774.",
            "A TMDb result's PremiereDate is the first air date at UTC midnight; upstream's \
         ToUniversalTime reads it as the server's local midnight, so on a host not on UTC \
         the instant differs by the offset. Fix path: gl-490.",
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) body is Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/BoxSet",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetBoxSetRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs box_set_remote_search (RequireAuth, the controller's \
               [Authorize]; JsonBody for the [Required] body, BoxSetInfo adding nothing to \
               ItemLookupInfo; to_request) → ferrofin-providers provider_manager.rs \
               remote_search (as for MusicAlbum: ProviderManager.GetRemoteSearchResults) → \
               TmdbBoxSetSearchProvider (TmdbBoxSetProvider.GetSearchResults, no IHasOrder so \
               50: a Tmdb id above 0 pins the collection through tmdb.rs collection \
               (GetCollectionAsync with GetImageLanguagesParam): its Name, the poster_path \
               poster and its Tmdb id, or nothing when TMDb has no such collection; else \
               search_collection by name in the normalized language (SearchCollectionAsync), \
               each row's Name, poster and Tmdb id, with no Overview)",
        date: "2026-11-14",
        divergences: &[
            "A TMDb id with surrounding whitespace or a leading `+` (` 119 `, `+119`) pins \
         the collection, and one past the 32-bit range answers nothing; v12.4's \
         TryParseTmdbId (NumberStyles.None into an int) reads both as no id and searches by \
         name. Fix path: gl-1774.",
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) body is Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/Person",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetPersonRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs person_remote_search (RequireAdminWithDefault: the \
               controller's [Authorize] and the action's Policies.RequiresElevation; JsonBody \
               for the [Required] body, PersonLookupInfo adding nothing to ItemLookupInfo; \
               to_request) → ferrofin-providers provider_manager.rs remote_search (as for \
               MusicAlbum: ProviderManager.GetRemoteSearchResults) → TmdbPersonSearchProvider \
               (TmdbPersonProvider.GetSearchResults, no IHasOrder so 50: a Tmdb id pins the \
               person through tmdb.rs person_lookup (GetPersonAsync in the normalized \
               language: Name, Biography as Overview, the first profile image, the Tmdb id and \
               the Imdb id TMDb knows), a failed lookup going on; else search_person by name \
               with no language (SearchPersonAsync, include_adult from the plugin settings), \
               each row's Name, profile image and Tmdb id)",
        date: "2026-11-15",
        divergences: &[
            "A TMDb id with surrounding whitespace or a leading sign (` 31 `, `+31`) pins the \
         person; v12.4's TryParseTmdbId (NumberStyles.None) reads it as no id and searches by \
         name. Fix path: gl-1774.",
            "TheTVDB's person search (jellyfin-plugin-tvdb TvdbPersonProvider.GetSearchResults) \
         is not ported, so a person Identify lists no TheTVDB candidates where Jellyfin with \
         the plugin does. Fix path: gl-1124.",
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) and 403 (not an administrator) bodies are Ferrofin's \
         error shape, not upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Items/RemoteSearch/Book",
        upstream_file: "Jellyfin.Api/Controllers/ItemLookupController.cs",
        upstream_method: "GetBookRemoteSearchResults",
        ferrofin: "handlers/item_lookup.rs book_remote_search (RequireAuth, the controller's \
               [Authorize]; JsonBody for the [Required] body; to_request carries BookInfo's \
               SeriesName) → ferrofin-providers provider_manager.rs remote_search (as for \
               MusicAlbum: ProviderManager.GetRemoteSearchResults), where no registered \
               provider supports Book, so the answer is []: v12.4's book providers \
               (MediaBrowser.Providers/Books) are local readers, with no \
               IRemoteSearchProvider<BookInfo>",
        date: "2026-11-14",
        divergences: &[
            "A body with no SearchInfo is searched as an empty lookup (200); upstream \
         dereferences the null SearchInfo and answers 500.",
            "The 401 (no or bad token) body is Ferrofin's error shape, not upstream's. Fix \
         path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Playback/BitrateTest",
        upstream_file: "Jellyfin.Api/Controllers/MediaInfoController.cs",
        upstream_method: "GetBitrateTestBytes",
        ferrofin: "handlers/media_info.rs get_bitrate_test (RequireAuth, the controller's \
               [Authorize]; crate::query::Query binds size case-insensitively, an empty value \
               refused as for the non-nullable int) → size defaults to 102400 and must be in \
               [1, 100000000] ([Range]), else 400 → size random bytes (rand::fill, \
               Random.Shared.NextBytes) as application/octet-stream \
               (MediaTypeNames.Application.Octet), which the response compression skips as \
               ResponseCompressionDefaults.MimeTypes does",
        date: "2026-11-13",
        divergences: &[
            "The body is exactly size bytes. Upstream sends the whole buffer it rents from \
         ArrayPool<byte>.Shared, which is rounded up to a power of two (about 131072 bytes \
         for the default 102400), so a client dividing the requested size by the download \
         time under-reports its bandwidth. Jellyfin bug not ported.",
            "Upstream returns that buffer to the pool in a finally before the File result is \
         written, so a concurrent request can rent and overwrite it mid-response. Ferrofin's \
         buffer belongs to its response. Jellyfin bug not ported.",
            "The 400 for a size out of range or that does not bind (upstream's \
         ValidationProblemDetails), and the 401 body, are Ferrofin's error shape, not \
         upstream's. Fix path: gl-666.",
        ],
        refs: &[],
    },
    Verified {
        method: "post",
        path: "/Startup/User",
        upstream_file: "Jellyfin.Api/Controllers/StartupController.cs",
        upstream_method: "UpdateStartupUser",
        ferrofin: "handlers/startup.rs update_startup_user (FirstTimeSetupOrElevated, the \
               controller's [Authorize]; JsonBody binds StartupUserDto, a missing or null Name \
               as None) → get_first_user, NotFound()'s ProblemDetails 404 when there is none → \
               403 when its password is neither null nor empty (Forbid) → 400 for a missing or \
               whitespace password (IsNullOrWhiteSpace) → ferrofin-core user_manager.rs \
               update_user (UpdateUserAsync) → rename_user (RenameUser, announcing \
               UserUpdated) only when the sent name differs from the stored one under \
               ferrofin-util string_extensions.rs equals_invariant_ignore_case \
               (InvariantCultureIgnoreCase) → change_password (ChangePassword) → 204",
        date: "2026-11-14",
        divergences: &[
            "The 403 (a password already set), 400 (no password, or a name the rename refuses \
         as invalid or taken) and, once the wizard is complete, 401 (no or bad token) and 403 \
         (not an administrator) bodies are Ferrofin's error shape, where upstream sends an \
         empty 401 and 403, the JSON string \"Password must not be empty\", and its exception \
         middleware's text/plain \"Error processing request.\" for the rename's 400. Fix \
         path: gl-666.",
            "A name ending in a newline (\"alice\\n\") is refused as invalid. Upstream's \
         ValidUsernameRegex ends in $, which .NET also matches before a final newline, so it \
         saves the name. Jellyfin bug not ported.",
            "A rename moves a profile image stored in the old name's directory to the new \
         name's directory, and a move that fails answers 500 and rolls the rename back. \
         Upstream's RenameUser leaves the file where it is, and a later user given the old \
         name then overwrites the renamed user's image. Jellyfin bug not ported.",
            "Setting the password writes no UserPasswordChanged activity entry, where \
         ChangePassword publishes UserPasswordChangedEventArgs. Fix path: gl-1602.",
            "The save ahead of the rename bumps Users.RowVersion, where UpdateUserAsync saves \
         the unchanged user without bumping it. Fix path: gl-1607.",
            "Names that differ only in character width or kana type (ｂｏｂ and bob, あき \
         and アキ) compare equal, so the stored name is kept, where .NET's ICU tailoring \
         tells them apart and upstream renames. Fix path: gl-1793.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Videos/{itemId}/hls/{playlistId}/{segmentId}",
        upstream_file: "Jellyfin.Api/Controllers/HlsSegmentController.cs",
        upstream_method: "GetHlsVideoSegmentLegacy",
        ferrofin: concat!(
            "handlers/hls.rs get_hls_video_segment_legacy (anonymous, as the action's \
             [Authenticated] is commented out; {segmentId}.{segmentContainer} split at its \
             last ., an empty side an empty 404 as no route matches upstream, before \
             blank_route_values' 400 naming every blank itemId, playlistId, segmentId and \
             segmentContainer; the file name the whole captured value, \
             string.Concat(segmentId, Path.GetExtension(Request.Path)), and both it and \
             playlistId through kestrel_route_value, which keeps an encoded slash as %2F as \
             Kestrel does) → ferrofin-hls hls_stream_manager.rs resolve_transcode_file → \
             resolve_in_transcode_dir (ValidateTranscodePath: a name that escapes the \
             transcode folder is a 400) → find_legacy_playlist (GetFilePaths(transcodePath) \
             with IgnoreInaccessible: the first file at the folder's top, in the order it \
             lists them, whose extension is .segmentContainer or .m3u8 and whose path \
             contains playlistId, OrdinalIgnoreCase; none is NotFound(\"Hls segment not \
             found.\")) → ",
            legacy_hls_tail!()
        ),
        date: "2026-11-15",
        divergences: &[
            LEGACY_HLS_BODIES,
            "The job kept active is the one whose playlist's file stem prefixes the found \
             file's, so a segment the folder happens to list before its playlist finds the job \
             too; upstream's OnTranscodeBeginRequest(playlistPath) finds one only when the first \
             match is a job's playlist itself. Response-invisible, and kept: a download is never \
             reaped either way the folder lists.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Videos/{itemId}/hls/{playlistId}/stream.m3u8",
        upstream_file: "Jellyfin.Api/Controllers/HlsSegmentController.cs",
        upstream_method: "GetHlsPlaylistLegacy",
        ferrofin: concat!(
            "handlers/hls.rs get_hls_playlist_legacy (RequireAuth, the action's [Authorize], \
             before the binder; {itemId} and {playlistId} through RequiredRouteString, the \
             binder's 400 for a blank one; the file name the whole playlistId through \
             kestrel_route_value and .m3u8) → ferrofin-hls hls_stream_manager.rs \
             resolve_transcode_file → resolve_in_transcode_dir (ValidateTranscodePath and the \
             .m3u8 check, a 400; GetFileResult(file, file)) → ",
            legacy_hls_tail!()
        ),
        date: "2026-11-15",
        divergences: &[
            LEGACY_HLS_BODIES,
            "The 401 (no or bad token) body is Ferrofin's error shape, where upstream sends an \
             empty 401. Fix path: gl-666.",
            "The job kept active is the one whose playlist's file stem prefixes the requested \
             playlist's, where upstream's OnTranscodeBeginRequest takes the job whose playlist \
             is that path exactly; they differ only when one job's name begins with another's. \
             Response-invisible.",
        ],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Audio/{itemId}/hls/{segmentId}/stream.aac",
        upstream_file: "Jellyfin.Api/Controllers/HlsSegmentController.cs",
        upstream_method: "GetHlsAudioSegmentLegacy",
        ferrofin: concat!(
            "handlers/hls.rs get_hls_audio_segment_legacy (anonymous, as the action's \
             [Authenticated] is commented out; {itemId} and {segmentId} through \
             RequiredRouteString, the binder's 400 for a blank one; the file name the whole \
             segmentId through kestrel_route_value and the path's extension, .aac) → \
             ferrofin-hls hls_stream_manager.rs resolve_transcode_file → \
             resolve_in_transcode_dir (ValidateTranscodePath, a 400) → ",
            legacy_hls_tail!()
        ),
        date: "2026-11-15",
        divergences: &[LEGACY_HLS_BODIES, LEGACY_HLS_AUDIO_KEEP_ALIVE],
        refs: &[],
    },
    Verified {
        method: "get",
        path: "/Audio/{itemId}/hls/{segmentId}/stream.mp3",
        upstream_file: "Jellyfin.Api/Controllers/HlsSegmentController.cs",
        upstream_method: "GetHlsAudioSegmentLegacy",
        ferrofin: concat!(
            "handlers/hls.rs get_hls_audio_segment_legacy (anonymous, as the action's \
             [Authenticated] is commented out; {itemId} and {segmentId} through \
             RequiredRouteString, the binder's 400 for a blank one; the file name the whole \
             segmentId through kestrel_route_value and the path's extension, .mp3) → \
             ferrofin-hls hls_stream_manager.rs resolve_transcode_file → \
             resolve_in_transcode_dir (ValidateTranscodePath, a 400) → ",
            legacy_hls_tail!()
        ),
        date: "2026-11-15",
        divergences: &[LEGACY_HLS_BODIES, LEGACY_HLS_AUDIO_KEEP_ALIVE],
        refs: &[],
    },
];

/// The two scheduled-task read rows: an adopted trigger file upstream cannot
/// load.
const ADOPTED_TRIGGER_FILE_DIVERGENCE: &str = "An adopted trigger file upstream's LoadTriggers \
     throws on does not stop Ferrofin: an empty file, malformed JSON (a syntax error, or a \
     value of the wrong shape), or a trigger type or weekday no enum member names leaves the task on its default triggers \
     (with a warning, except for an empty file), and a trigger without the time of day, \
     weekday or interval its type needs is kept and never fires; Jellyfin fails to start. \
     Jellyfin bug not ported.";

/// The two scheduled-task read rows: the System.Text.Json leniencies a file
/// read here does not mirror.
const ADOPTED_FILE_LENIENCY_DIVERGENCE: &str = "An adopted task file that leans on \
     System.Text.Json's leniency (a trigger without Type, a numeric Status, a number written \
     as a string) is skipped as unreadable, where upstream reads it; Jellyfin never writes \
     one. Fix path: gl-1845.";

/// What `/Artists` and `/Artists/AlbumArtists` do differently from v12's
/// `ArtistsController` + `BaseItemRepository.GetItemValues`.
const BY_NAME_DIVERGENCES_ARTISTS: &[&str] = &[
    "`studios` names resolve without writing: v12's LibraryManager.GetStudio CREATES a row \
     for an unknown name on this GET; Ferrofin's lookup creates none and gives the name a \
     no-match id (by_name::resolve_by_name_ids), the same empty answer.",
    NO_USER_USER_DATA_FILTER_DIVERGENCE,
    "A `userId` naming no user: v12's GetUserById returns null and the query runs with no \
     user — unscoped, every library on the server; Ferrofin answers 404. Jellyfin bug not \
     ported.",
    "With `filters=IsPlayed`/`IsUnplayed` and counts requested, a listed row whose carriers \
     are all on the other side of the played filter gets the ten count fields as 0 where v12 \
     omits them (its `ItemCounts?` is null for a name the count scope never saw). \
     Un-defer path: make `ItemWithCounts.counts` an `Option` in ferrofin-traits.",
    "`person`, `personIds`, `personTypes` and a valid `minCommunityRating` are accepted \
     and ignored, exactly as v12 — GetItemValues copies none of them onto either filter; a \
     `minCommunityRating` that is not a double is the binder's 400 in both.",
    "The 400 (a value the query binder cannot convert, a parentId naming no item, or a \
     conflicting `filters` pair) and 403 (another user's id without the administrator \
     role) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
    SEARCH_TERM_ORIGINAL_TITLE_DIVERGENCE,
];

/// What `/Genres`, `/MusicGenres` and `/Studios` do differently from their v12
/// controllers + `GetItemValues`. They bind only the parameters those
/// controllers declare (`by_name.rs` `ValueListQuery`).
const BY_NAME_DIVERGENCES_GENRES_STUDIOS: &[&str] = &[
    "The 400 (a value the query binder cannot convert, or a parentId naming no item) and \
     403 (another user's id without the administrator role) bodies are Ferrofin's error \
     shape, not upstream's. Fix path: gl-666.",
    "A `userId` naming no user: v12's GetUserById returns null and the query runs with no \
     user — unscoped, every library on the server; Ferrofin answers 404. Jellyfin bug not \
     ported.",
    NO_USER_USER_DATA_FILTER_DIVERGENCE,
    SEARCH_TERM_ORIGINAL_TITLE_DIVERGENCE,
];

/// `searchTerm`'s `OriginalTitle` arm (`translate_query::push_search_term`),
/// shared by a browse and the by-name lists (gl-957). Otherwise the predicate
/// is v12.4's, checked against a stock 12.4's `/Genres` for `%`, `_`, `^`,
/// `[`/`]`, a whitespace-only term, terms that clean to nothing and an
/// `OriginalTitle` match.
const SEARCH_TERM_ORIGINAL_TITLE_DIVERGENCE: &str = "`searchTerm`'s OriginalTitle arm \
     lower-cases both sides (Unicode); v12.4 TranslateQuery matches the raw term with SQLite \
     LIKE, which folds ASCII case only, so an OriginalTitle whose non-ASCII letters differ in \
     case from the term matches here and not upstream (a Name match folds them on both, \
     through CleanName). Jellyfin bug not ported (gl-957).";

/// Included in the divergence lists of the five by-name VERIFIED rows: an API
/// key with no `userId` lists them with no user.
const NO_USER_USER_DATA_FILTER_DIVERGENCE: &str = "A user-data filter (`isFavorite`, or on \
     /Artists a user-data `filters` flag: IsFavorite, IsFavoriteOrLikes, Likes, Dislikes, \
     IsPlayed, IsUnplayed) with no user (an API key without userId) is ignored and the list is \
     unfiltered (200); upstream's TranslateQuery dereferences the null User for the user-data \
     subqueries and fails (500). Jellyfin bug not ported.";

/// The divergences of the two playback-start reports (`POST /Sessions/Playing`,
/// `POST /PlayingItems/{itemId}`).
const PLAYBACK_START_DIVERGENCES: &[&str] = &[
    NOW_PLAYING_ITEM_BUILT_ON_READ,
    "SessionInfo.StartAutomaticProgress is not run, so the session's position does not \
     advance between reports. Fix path: gl-803.",
    "The activity entry is written in English, not the server's UI culture. Fix path: gl-429.",
    "The 400 bodies (a body or query member that does not bind) are Ferrofin's error shape. \
     Fix path: gl-666.",
];

/// The divergences of the two playback-progress reports.
const PLAYBACK_PROGRESS_DIVERGENCES: &[&str] = &[
    NOW_PLAYING_ITEM_BUILT_ON_READ,
    "SessionInfo.StartAutomaticProgress is not re-armed by a report, so no automated progress \
     follows it. Fix path: gl-803.",
    "The 400 bodies (a body or query member that does not bind) are Ferrofin's error shape. \
     Fix path: gl-666.",
];

/// The divergences of the two playback-stop reports.
const PLAYBACK_STOP_DIVERGENCES: &[&str] = &[
    "The activity entry is written in English, not the server's UI culture. Fix path: gl-429.",
    "The live stream is released and the stop published even when the user-data save fails, \
     and the item is always read again rather than taken from the session, so an item deleted \
     mid-play saves nothing; upstream skips the release when the save throws and throws on the \
     deleted item. Jellyfin bug not ported.",
    "The 400 bodies (a negative position, a body or query member that does not bind) are \
     Ferrofin's error shape. Fix path: gl-666.",
    "A job started on a stopped job's output path while that job's partial-file deletion \
     waits keeps its files, as on DELETE /Videos/ActiveEncodings; upstream deletes them. \
     Jellyfin bug not ported (gl-1876).",
];

/// Where a session's `NowPlayingItem` is built (owner decision 2026-11-04,
/// gl-1242): shared by the playback reports, `GET /Sessions` and the Sessions
/// stream.
const NOW_PLAYING_ITEM_BUILT_ON_READ: &str = "A playback report keeps the item it names, as its \
     row read, and the first GET /Sessions, Sessions stream push or SyncPlay group creation \
     that shows it builds the NowPlayingItem, which later ones copy; upstream builds it before the report returns. The \
     item's own fields are those of the row the report read, and its streams, images and \
     chapters are those the first listing finds. Owner decision (gl-1242, 2026-11-04).";

/// The accepted divergences of `GET /Sessions`.
const SESSIONS_DIVERGENCES: &[&str] = &[
    NOW_PLAYING_ITEM_BUILT_ON_READ,
    "A NowPlayingItem whose media source lookup fails is shown without its streams, and one \
     whose build fails is not shown, while the report itself has succeeded and saved the \
     user's progress; the first listing after the next report naming the item builds it \
     again. Upstream fails the report, which keeps the session's previous NowPlayingItem.",
    "A non-administrator's response carries the hardware accelerator blanked, and only that \
     response; upstream blanks it on the stored session's TranscodingInfo, so every later \
     reader, an administrator included, sees it blanked until the next transcoding report \
     replaces it (the websocket-streams row records the Sessions stream's side). Jellyfin \
     bug not ported.",
    "The 403 (controllableByUserId naming another user without the administrator role) and \
     400 (a value the query binder cannot convert) bodies are Ferrofin's error shape, not \
     upstream's. Fix path: gl-666.",
    "A session with no WebSocket that plays nothing is ended once it has made no request for \
     longer than the session_idle_hours setting (24 by default, 1 to 168), through the path a \
     closed socket takes (SessionEnded and its activity-log entry); upstream keeps such a \
     session until the server restarts. A client that returns gets a new session with the \
     same id and its device's capabilities (the map is never pruned, as upstream's \
     _capabilitiesMap), so only what the ended session held (AdditionalUsers, NowViewingItem, \
     the last NowPlayingQueue, PlaylistItemId and pause, LastPlaybackCheckIn) starts over. \
     Owner decision (gl-1267, 2026-11-05).",
];

/// `GET /Users`, `/Users/Public` and `/Users/{userId}` leave other accounts'
/// private fields out (handlers/users.rs strip_other_accounts).
const USER_FIELDS_STRIPPED: &str = "Policy and Configuration are left out of every row but the \
     caller's own, unless the caller is an administrator or an API key, and out of every row \
     for an anonymous caller. Upstream sends both to every caller. Owner decision (gl-1102, \
     2026-10-31).";

/// `UserManager.GetUserDto` never copies this permission into its `UserPolicy`.
const USER_LYRIC_MANAGEMENT: &str = "Policy.EnableLyricManagement reads the stored permission. \
     Upstream's GetUserDto leaves it out, so it always reads false, and a client saving the \
     policy it read clears the permission. Jellyfin bug not ported.";

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
     count each leaf's own played row; v12.4's ItemCountService counts a leaf played \
     through an alternate version (DescendantQueryHelper.IsPlayedBy). Fix path: the \
     alternate-version arm in item_count_service's played statements (gl-441).",
    "Resume positions that Ferrofin builds before gl-46 saved for songs and for Sample, \
     ThemeVideo and Trailer extras (kinds v12 never lets resume) keep those items listed \
     until they are played again; new plays no longer save one. Fix path: gl-717.",
    SEARCH_TERM_ORIGINAL_TITLE_DIVERGENCE,
    "The 400 (an API key with no userId, or a value the query binder cannot convert), \
     403 (another user's id without the administrator role) and 404 (a userId naming no \
     account) bodies are Ferrofin's error shape, not upstream's. Fix path: gl-666.",
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

/// Mounts every real handler onto `router`.
///
/// Called by [`create_router`](crate::router::create_router) after its stub
/// loop, which skips the `501` stub for each [`REAL_ROUTES`] row, so a real
/// handler is the sole route for its `(method, path)`. A path outside the
/// contract is listed in [`UNLISTED_ROUTES`] (or [`PLUGIN_ROUTES`]), so it
/// matches ignoring case.
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
    // Similar items (`LibraryController.GetSimilarItems` and its aliases).
    let router = similar::register(router);
    // MergeVersions plugin — bulk merge/split of duplicate versions.
    let router = merge_versions::register(router);
    // Plugins Tier 1 — plugin-manager surface over the compile-time registry.
    let router = plugins::register(router);
    // The Ferrofin plugin's settings page — not a contract operation, so in
    // neither REAL_ROUTES nor EXTENSION_ROUTES.
    let router = ferrofin_settings::register(router);
    // The LDAP-Auth plugin's own controller — not a contract operation, so
    // listed in PLUGIN_ROUTES.
    let router = ldap::register(router);
    // The Single Sign-On plugin's own routes — not contract operations, so
    // listed in PLUGIN_ROUTES.
    let router = sso::register(router);
    // Live TV over the `LiveTvManager`; with nothing configured the reads answer
    // the empty state jellyfin-web shows as "not configured".
    let router = live_tv::register(router);
    // SyncPlay — synchronized group playback over the session WebSocket.
    let router = sync_play::register(router);
    // The session WebSocket (`/socket`) — not in the OpenAPI contract; jellyfin-web
    // needs it to establish a connection or it reports "Connection Failure".
    websocket::register(router)
}

#[cfg(test)]
mod tests {
    use super::run_detached;

    /// The detached work runs inside the request's span, so its logs and
    /// spans stay under `http_request` (LOGGING.md §4), and under the
    /// request's culture.
    #[tokio::test]
    async fn detached_work_keeps_the_requests_span_and_culture() {
        use tracing::Instrument as _;
        let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry());
        let span = tracing::info_span!("http_request");
        let seen = ferrofin_util::current_culture::scope(
            "sv".to_owned(),
            run_detached(async {
                let span = tracing::Span::current()
                    .metadata()
                    .map(tracing::Metadata::name);
                Ok::<_, crate::error::ApiError>((span, ferrofin_util::current_culture::capture()))
            }),
        )
        .instrument(span)
        .await
        .expect("ran");
        assert_eq!(seen, (Some("http_request"), "sv".to_owned()));
    }
}
