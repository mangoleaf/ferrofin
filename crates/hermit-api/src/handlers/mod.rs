//! Real ported handlers for the First-Light routes.
//!
//! Each submodule mirrors one `Jellyfin.Api` controller and holds axum handlers
//! that call the [`AppState`](crate::state::AppState) manager traits, project
//! results through [`DtoService`](hermit_traits::dto::DtoService), and return
//! the wire DTOs from `hermit-model`. These are the routes with *real* behaviour
//! (the rest of the contract stays on the shared `not_implemented` `501` stub);
//! [`register`] mounts them over their stub entries.
//!
//! Handlers behind Jellyfin's `[Authorize]` policy take the
//! [`RequireAuth`](crate::auth::RequireAuth) extractor (a missing/invalid token
//! becomes `401`); public routes read the (possibly anonymous)
//! [`AuthorizationInfo`](hermit_traits::options::AuthorizationInfo) extension set
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
    use hermit_traits::providers::{MetadataRefreshOptions, RefreshPriority};
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
pub mod branding;
pub mod by_name;
pub mod client_log;
pub mod collection;
pub mod config;
pub mod dashboard;
pub mod devices;
pub mod display_preferences;
pub mod environment;
pub mod filter;
pub mod genres;
pub mod images;
pub mod instant_mix;
pub mod item_lookup;
pub mod item_update;
pub mod items;
pub mod library;
pub mod localization;
pub mod lyrics;
pub mod media_info;
pub mod media_segments;
pub mod movies;
pub mod music_genres;
pub mod persons;
pub mod playlists;
pub mod playstate;
pub(crate) mod query_parse;
pub mod quick_connect;
pub mod remote_images;
pub mod scheduled_tasks;
pub mod search;
pub mod session;
pub(crate) mod session_ctx;
pub mod startup;
pub(crate) mod streaming;
pub mod studios;
pub mod subtitles;
pub mod suggestions;
pub mod system;
pub mod time_sync;
pub mod trailers;
pub mod trickplay;
pub mod tv_shows;
pub mod user_library;
pub mod user_views;
pub mod users;
pub mod videos;
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
    // Live streams + bitrate test (MediaInfoController).
    ("post", "/LiveStreams/Open"),
    ("post", "/LiveStreams/Close"),
    ("get", "/Playback/BitrateTest"),
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
    // Deferred (stay on the 501 stub): on-the-fly subtitle conversion
    // (`Videos/{itemId}/{container}/Subtitles/{index}/{routeFormat}` + the HLS
    // `subtitles.m3u8` playlist — need the un-ported SubtitleEncoder); the
    // FallbackFont routes (encoding-options config not surfaced at the config
    // seam); and `/MediaSegmentsApi/*` (plugin host).
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
    // Branding reads (Splashscreen GET/POST/DELETE stay on the 501 stub).
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
    // library-scan trigger. The virtual-folder mutation/read routes
    // (`/Library/VirtualFolders*`, `/Library/PhysicalPaths`,
    // `/Library/MediaFolders`, `/Libraries/AvailableOptions`), the
    // external-source change reports (`/Library/Series|Movies|Media/*`), and the
    // remote-metadata search/apply routes (`/Items/RemoteSearch/*`) stay on the
    // 501 stub — each needs an unported subsystem (on-disk collection-folder
    // tree + LibraryOptions, the filesystem monitor, or network metadata
    // fetchers). See `handlers::library` / `handlers::item_lookup`.
    ("get", "/Items/{itemId}/ThemeSongs"),
    ("get", "/Items/{itemId}/ThemeVideos"),
    ("get", "/Items/{itemId}/ThemeMedia"),
    ("get", "/Items/{itemId}/File"),
    ("post", "/Library/Refresh"),
    ("get", "/Items/{itemId}/ExternalIdInfos"),
    // Batch 15 — ScheduledTasks read/run.
    // List, fetch-by-id, and manual run-now. The scheduler-cron machinery stays
    // on the 501 stub: `DELETE /ScheduledTasks/Running/{taskId}` (cancel — no
    // background run to cancel) and `POST /ScheduledTasks/{taskId}/Triggers`
    // (trigger-config persistence). Channels (`ChannelsController`) stays fully
    // on the 501 stub as a deferred Live-TV/channels subsystem.
    ("get", "/ScheduledTasks"),
    ("get", "/ScheduledTasks/{taskId}"),
    ("post", "/ScheduledTasks/Running/{taskId}"),
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
    let router = item_lookup::register(router);
    let router = system::register(router);
    // Batch 15 — ScheduledTasks read/run.
    scheduled_tasks::register(router)
}
