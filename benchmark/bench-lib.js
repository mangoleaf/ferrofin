// Shared k6 helpers for the load scripts (scenario.js, phase-*.js, bootstrap.js). The fiddly
// first-boot + provisioning sequence — with all its 10.11 gotchas (modern auth grammar only,
// JSON body required, startup-wizard race) — lives here ONCE so the two scripts can't drift.
// Every function takes the target's base URL explicitly, so one script can drive both servers.
import http from 'k6/http';
import { check, sleep } from 'k6';

const USER = __ENV.BENCH_ADMIN_USER || 'bench';
const PASS = __ENV.BENCH_ADMIN_PASSWORD || 'benchpass123';

// Modern `Authorization: MediaBrowser …` grammar only: 10.11 ships EnableLegacyAuthorization=
// false, so X-Emby-Token/X-Emby-Authorization are rejected by a fresh install of either server.
const CLIENT_ID = 'Client="bench", Device="bench", DeviceId="bench", Version="1.0"';

export function tokenHeaders(token) {
  return { headers: { Authorization: `MediaBrowser Token="${token}", ${CLIENT_ID}`, 'Content-Type': 'application/json' } };
}

export function authenticate(base, target) {
  const r = http.post(`${base}/Users/AuthenticateByName`,
    JSON.stringify({ Username: USER, Pw: PASS }),
    { headers: { 'Content-Type': 'application/json', Authorization: `MediaBrowser ${CLIENT_ID}` } });
  check(r, { 'auth 200': (x) => x.status === 200 });
  if (r.status !== 200) throw new Error(`[${target}] auth failed: ${r.status} ${String(r.body).slice(0, 200)}`);
  const b = r.json();
  return { token: b.AccessToken, userId: b.User.Id };
}

// `includeTypes` omitted ⇒ count ALL item types (recursive). Progress polling must use the
// unfiltered count: it climbs steadily as rows are indexed, whereas the Movie,Episode-filtered
// count lags on Hermit (items are classified late in the scan) and can sit flat for ~20s during a
// single slow 4K ffprobe — which used to make waitForScan settle prematurely (e.g. at 3 items).
export function itemCount(base, ctx, includeTypes) {
  const t = includeTypes ? `&includeItemTypes=${includeTypes}` : '';
  const r = http.get(`${base}/Items?userId=${ctx.userId}&recursive=true${t}&limit=0`, tokenHeaders(ctx.token));
  return r.status === 200 ? (r.json().TotalRecordCount || 0) : -1;
}

// Poll the unfiltered total until it stops growing, then report the Movie,Episode count. Shared,
// API-defined completion signal — no scan-status API needed, works identically on both servers.
export function waitForScan(base, target, ctx, _expected) {
  let last = -1, stable = 0, zeros = 0;
  for (let i = 0; i < 480; i++) {          // 480*5s = 40min cap, under the 45m setupTimeout
    const n = itemCount(base, ctx);        // all types — the steady progress signal
    console.log(`[${target}] scan progress: ${n} items`);
    stable = (n === last && n > 0) ? stable + 1 : 0;
    // Settle only after the total holds for ~40s (8 polls): a single large 4K ffprobe can pause
    // growth ~20s, so a shorter window false-settles mid-scan. STABLE_POLLS is the knob.
    if (stable >= 8) break;
    zeros = n <= 0 ? zeros + 1 : 0;
    if (zeros >= 36) throw new Error(`[${target}] still 0 items after 3 minutes — scan never started`);
    last = n;
    sleep(5);
  }
  // Movie,Episode is the fair figure for the report (folders resolve differently per server).
  return itemCount(base, ctx, 'Movie,Episode');
}

// Add the libraries from the LIBRARIES env (real media and/or synthetic padding) and kick a scan.
export function provision(base, target, ctx) {
  const h = tokenHeaders(ctx.token);
  // Fairness: Hermit's remote metadata providers are inert (feature-gated / no keys), so
  // Jellyfin's must be off too — else its TMDB/OMDb fetchers stay on (slower, network-dependent,
  // richer DTOs = a different workload). Empty fetcher arrays disable them; local NFO/image stay on.
  const noRemote = {
    LibraryOptions: {
      EnableRealtimeMonitor: false,
      SaveLocalMetadata: false,
      TypeOptions: ['Movie', 'Series', 'Season', 'Episode'].map((t) => (
        { Type: t, MetadataFetchers: [], MetadataFetcherOrder: [], ImageFetchers: [], ImageFetcherOrder: [] })),
    },
  };
  for (const l of JSON.parse(__ENV.LIBRARIES || '[]')) {
    const q = `name=${encodeURIComponent(l.name)}&collectionType=${l.type}&paths=${encodeURIComponent(l.path)}`;
    // Always send a real JSON body: an empty body with a JSON content-type is a 400 on Hermit.
    const body = target === 'jellyfin' ? JSON.stringify(noRemote) : '{}';
    const r = http.post(`${base}/Library/VirtualFolders?${q}${target === 'jellyfin' ? '&refreshLibrary=true' : ''}`, body, h);
    if (r.status >= 300) throw new Error(`[${target}] add library "${l.name}" failed: ${r.status} ${r.body}`);
  }
  if (target !== 'jellyfin') {
    const r = http.post(`${base}/Library/Refresh`, null, h);   // hermit: kick the scan
    if (r.status >= 300) throw new Error(`[${target}] /Library/Refresh failed: ${r.status}`);
  }
}

// Jellyfin's first boot needs the startup wizard completed before AuthenticateByName works.
// /System/Info/Public 200s while migrations are still seeding, so retry until Complete sticks.
export function jellyfinFirstRunWizard(base, target) {
  const jh = { headers: { 'Content-Type': 'application/json' } };
  for (let i = 0; i < 60; i++) {
    const cfg = http.post(`${base}/Startup/Configuration`,
      JSON.stringify({ UICulture: 'en-US', MetadataCountryCode: 'US', PreferredMetadataLanguage: 'en' }), jh);
    if (cfg.status < 300) {
      http.get(`${base}/Startup/User`);
      http.post(`${base}/Startup/User`, JSON.stringify({ Name: USER, Password: PASS }), jh);
      const done = http.post(`${base}/Startup/Complete`, null, jh);
      if (done.status < 300) return;
    }
    sleep(2);
  }
  throw new Error(`[${target}] startup wizard never completed`);
}

// Provision one server end-to-end and return its ready ctx (token, userId, itemsFound).
export function bringUp(base, target, expected) {
  if (target === 'jellyfin') jellyfinFirstRunWizard(base, target);
  const ctx = authenticate(base, target);
  provision(base, target, ctx);
  ctx.itemsFound = waitForScan(base, target, ctx, expected);
  return ctx;
}

// The read (GET) endpoints both scripts exercise. `path(ctx)` templates per-server ids.
// image is best-effort in the load bench; parity skips it (binary — resize output differs by lib).
export const ENDPOINTS = [
  // Framework floor — near-zero work, isolates routing/serialization overhead.
  { name: 'info_public', auth: false, path: () => '/System/Info/Public' },
  { name: 'system_info', path: () => '/System/Info' },
  { name: 'system_endpoint', path: () => '/System/Endpoint' },
  { name: 'localization_cultures', path: () => '/Localization/Cultures' },
  { name: 'user_me', path: () => '/Users/Me' },
  { name: 'sessions', path: () => '/Sessions' },
  { name: 'scheduled_tasks', path: () => '/ScheduledTasks' },
  { name: 'plugins', path: () => '/Plugins' },
  { name: 'media_folders', path: () => '/Library/MediaFolders' },
  { name: 'virtual_folders', path: () => '/Library/VirtualFolders' },
  // Home-screen assembly.
  { name: 'user_views', path: (c) => `/UserViews?userId=${c.userId}` },
  { name: 'items_latest', path: (c) => `/Items/Latest?userId=${c.userId}&limit=20` },
  { name: 'items_resume', path: (c) => `/UserItems/Resume?userId=${c.userId}&limit=12` },
  { name: 'nextup', path: (c) => `/Shows/NextUp?userId=${c.userId}&limit=24` },
  { name: 'upcoming', path: (c) => `/Shows/Upcoming?userId=${c.userId}&limit=24` },
  // Library query + DTO hot path — the query planner + PascalCase serialization under load.
  { name: 'items_sortname', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=SortName` },
  { name: 'items_datesort', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=DateCreated&sortOrder=Descending` },
  { name: 'items_episodes', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=Episode&limit=50&sortBy=SortName` },
  { name: 'items_series', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=Series&limit=50&sortBy=SortName` },
  { name: 'items_mixed', path: (c) => `/Items?userId=${c.userId}&recursive=true&limit=100&sortBy=SortName` },
  // Faceted browse — GROUP BY / DISTINCT paths over the item set.
  { name: 'genres', path: (c) => `/Genres?userId=${c.userId}` },
  { name: 'persons', path: (c) => `/Persons?userId=${c.userId}&limit=100` },
  { name: 'studios', path: (c) => `/Studios?userId=${c.userId}` },
  { name: 'years', path: (c) => `/Years?userId=${c.userId}` },
  { name: 'filters', path: (c) => `/Items/Filters?userId=${c.userId}&includeItemTypes=Movie` },
  { name: 'search_hints', path: (c) => `/Search/Hints?userId=${c.userId}&searchTerm=a&limit=20` },
  // Single-item detail + related.
  { name: 'item_detail', path: (c) => `/Items/${c.itemId}?userId=${c.userId}` },
  { name: 'item_ancestors', path: (c) => `/Items/${c.itemId}/Ancestors?userId=${c.userId}` },
  { name: 'item_images', path: (c) => `/Items/${c.itemId}/Images` },
  { name: 'item_similar', path: (c) => `/Items/${c.itemId}/Similar?userId=${c.userId}&limit=12` },

  // ── Broader read surface ──────────────────────────────────────────────────
  // Every endpoint below is a GET the parity ledger certifies both servers answer
  // 200 (parity/ledger.json, note "H=200 J=200"), needing only userId/itemId — so
  // no extra setup ids. suite/registry.json mirrors this list keyed by contract op. Grouped by subsystem.

  // System / diagnostics / meta.
  { name: 'system_ping', path: () => '/System/Ping' },
  { name: 'system_storage', path: () => '/System/Info/Storage' },
  { name: 'system_config', path: () => '/System/Configuration' },
  { name: 'metadata_options', path: () => '/System/Configuration/MetadataOptions/Default' },
  { name: 'activity_log', path: () => '/System/ActivityLog/Entries' },
  { name: 'system_logs', path: () => '/System/Logs' },
  { name: 'utc_time', path: () => '/GetUtcTime' },

  // Localization / branding / auth dictionaries (cheap, mostly-static payloads).
  { name: 'loc_countries', path: () => '/Localization/Countries' },
  { name: 'loc_options', path: () => '/Localization/Options' },
  { name: 'loc_parental', path: () => '/Localization/ParentalRatings' },
  { name: 'branding', path: () => '/Branding/Configuration' },
  { name: 'auth_providers', path: () => '/Auth/Providers' },
  { name: 'auth_pw_providers', path: () => '/Auth/PasswordResetProviders' },
  { name: 'auth_keys', path: () => '/Auth/Keys' },
  { name: 'quick_connect', path: () => '/QuickConnect/Enabled' },

  // Users / devices / view grouping.
  { name: 'users_all', path: () => '/Users' },
  { name: 'users_public', path: () => '/Users/Public' },
  { name: 'devices', path: () => '/Devices' },
  { name: 'grouping_options', path: () => '/UserViews/GroupingOptions' },

  // Library configuration + faceted browse.
  { name: 'physical_paths', path: () => '/Library/PhysicalPaths' },
  { name: 'available_options', path: () => '/Libraries/AvailableOptions' },
  { name: 'items_counts', path: (c) => `/Items/Counts?userId=${c.userId}` },
  { name: 'items_filters2', path: (c) => `/Items/Filters2?userId=${c.userId}&includeItemTypes=Movie` },
  { name: 'suggestions', path: (c) => `/Items/Suggestions?userId=${c.userId}&limit=20` },
  { name: 'artists', path: (c) => `/Artists?userId=${c.userId}` },
  { name: 'album_artists', path: (c) => `/Artists/AlbumArtists?userId=${c.userId}` },
  { name: 'music_genres', path: (c) => `/MusicGenres?userId=${c.userId}` },
  { name: 'movie_recommendations', path: (c) => `/Movies/Recommendations?userId=${c.userId}&categoryLimit=6&itemLimit=8` },

  // Query-planner variety — same /Items path, different sort/filter/paging shapes.
  { name: 'items_random', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=Random` },
  { name: 'items_rating', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=CommunityRating&sortOrder=Descending` },
  { name: 'items_paged', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=Movie&startIndex=100&limit=50&sortBy=SortName` },
  { name: 'items_boxset', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=BoxSet&limit=50` },
  { name: 'items_favorite', path: (c) => `/Items?userId=${c.userId}&recursive=true&includeItemTypes=Movie&limit=50&filters=IsFavorite` },

  // Channels / SyncPlay / Live TV subsystem handlers (return empty defaults, but
  // exercise the real route + serializer on both servers).
  { name: 'channels', path: () => '/Channels' },
  { name: 'syncplay_list', path: () => '/SyncPlay/List' },
  { name: 'livetv_info', path: () => '/LiveTv/Info' },
  { name: 'livetv_channels', path: () => '/LiveTv/Channels' },
  { name: 'livetv_programs', path: () => '/LiveTv/Programs' },
  { name: 'livetv_recordings', path: () => '/LiveTv/Recordings' },
  { name: 'livetv_series_timers', path: () => '/LiveTv/SeriesTimers' },
  { name: 'livetv_timers', path: () => '/LiveTv/Timers' },

  // Item detail sub-resources (all keyed on the picked movie itemId).
  { name: 'item_playbackinfo', path: (c) => `/Items/${c.itemId}/PlaybackInfo?userId=${c.userId}` },
  { name: 'item_external_ids', path: (c) => `/Items/${c.itemId}/ExternalIdInfos` },
  { name: 'item_critic_reviews', path: (c) => `/Items/${c.itemId}/CriticReviews` },
  { name: 'item_intros', path: (c) => `/Items/${c.itemId}/Intros?userId=${c.userId}` },
  { name: 'item_special_features', path: (c) => `/Items/${c.itemId}/SpecialFeatures?userId=${c.userId}` },
  { name: 'item_local_trailers', path: (c) => `/Items/${c.itemId}/LocalTrailers?userId=${c.userId}` },
  { name: 'item_theme_media', path: (c) => `/Items/${c.itemId}/ThemeMedia?userId=${c.userId}` },
  { name: 'item_instant_mix', path: (c) => `/Items/${c.itemId}/InstantMix?userId=${c.userId}&limit=20` },
  { name: 'item_userdata', path: (c) => `/UserItems/${c.itemId}/UserData?userId=${c.userId}` },
  { name: 'item_similar_movie', path: (c) => `/Movies/${c.itemId}/Similar?userId=${c.userId}&limit=12` },
  { name: 'media_segments', path: (c) => `/MediaSegments/${c.itemId}` },

  // Image serve + resize (hermit-drawing). Best-effort: N/A if no local poster is discovered.
  { name: 'image_primary', path: (c) => `/Items/${c.imageItemId}/Images/Primary?fillHeight=400&fillWidth=400` },

  // ── Expanded surface (2026-08: 43/412 benched ops was ~10%) ──────────────
  // Every entry is a ledger-certified GET whose params resolve from the
  // enriched context (enrichContext below). Grouped by subsystem.

  // TV browse — the Shows tab's two heavy queries.
  { name: 'shows_episodes', path: (c) => `/Shows/${c.seriesId}/Episodes?userId=${c.userId}&limit=50` },
  { name: 'shows_seasons', path: (c) => `/Shows/${c.seriesId}/Seasons?userId=${c.userId}` },

  // Item detail sub-resources (continued).
  { name: 'item_theme_songs', path: (c) => `/Items/${c.itemId}/ThemeSongs?userId=${c.userId}` },
  { name: 'item_theme_videos', path: (c) => `/Items/${c.itemId}/ThemeVideos?userId=${c.userId}` },
  { name: 'video_additional_parts', path: (c) => `/Videos/${c.itemId}/AdditionalParts?userId=${c.userId}` },

  // Image routes: indexed and the fully-parametrized (path-baked transform) form.
  { name: 'item_image_indexed', path: (c) => `/Items/${c.imageItemId}/Images/Primary/0` },
  { name: 'item_image_parametrized', path: (c) => `/Items/${c.imageItemId}/Images/Primary/0/${c.imageTag}/webp/300/450/0/0` },

  // Playlists (created once by enrichContext).
  { name: 'playlist_detail', path: (c) => `/Playlists/${c.playlistId}` },
  { name: 'playlist_items', path: (c) => `/Playlists/${c.playlistId}/Items?userId=${c.userId}` },
  { name: 'playlist_users', path: (c) => `/Playlists/${c.playlistId}/Users` },

  // Branding / fonts / startup / environment / system.
  { name: 'branding_css', path: () => '/Branding/Css' },
  { name: 'branding_css_ext', path: () => '/Branding/Css.css' },
  { name: 'fallback_fonts', path: () => '/FallbackFont/Fonts' },
  { name: 'startup_config', path: () => '/Startup/Configuration' },
  { name: 'startup_first_user', path: () => '/Startup/FirstUser' },
  { name: 'startup_user', path: () => '/Startup/User' },
  { name: 'env_default_browser', path: () => '/Environment/DefaultDirectoryBrowser' },
  { name: 'env_network_shares', path: () => '/Environment/NetworkShares' },
  { name: 'system_config_key', path: () => '/System/Configuration/encoding' },
  { name: 'backup_list', path: () => '/Backup' },
  { name: 'tmdb_config', path: () => '/Tmdb/ClientConfiguration' },

  // Devices / users / tasks (ids from the enriched context).
  { name: 'devices_info', path: () => '/Devices/Info?id=bench' },
  { name: 'user_by_id', path: (c) => `/Users/${c.userId}` },
  { name: 'scheduled_task_detail', path: (c) => `/ScheduledTasks/${c.taskId}` },

  // Channels + Live TV read surface (empty-but-real on the bench library).
  { name: 'channels_features', path: () => '/Channels/Features' },
  { name: 'channels_latest', path: (c) => `/Channels/Items/Latest?userId=${c.userId}` },
  { name: 'livetv_programs_recommended', path: (c) => `/LiveTv/Programs/Recommended?userId=${c.userId}` },
  { name: 'livetv_recording_folders', path: (c) => `/LiveTv/Recordings/Folders?userId=${c.userId}` },
  { name: 'livetv_recordings_series', path: (c) => `/LiveTv/Recordings/Series?userId=${c.userId}` },
  { name: 'livetv_recording_groups', path: () => '/LiveTv/Recordings/Groups' },
  { name: 'livetv_listing_default', path: () => '/LiveTv/ListingProviders/Default' },
];

// Resolves the extra context ids the expanded endpoint set needs, after auth +
// scan: the first series (TV browse), a scheduled task id, the picked image
// item's cache tag, and a bench playlist (created if absent — fresh DB per
// run). Fields default to '' so a library without that shape yields 404s on
// both servers identically rather than breaking the run.
export function enrichContext(base, ctx) {
  const h = tokenHeaders(ctx.token);
  const series = http.get(`${base}/Items?userId=${ctx.userId}&recursive=true&includeItemTypes=Series&sortBy=SortName&limit=1`, h).json();
  ctx.seriesId = (series.Items && series.Items[0]) ? series.Items[0].Id : '';
  const tasks = http.get(`${base}/ScheduledTasks`, h).json();
  ctx.taskId = (tasks && tasks[0]) ? (tasks[0].Id || tasks[0].Key) : '';
  const img = http.get(`${base}/Items?userId=${ctx.userId}&recursive=true&includeItemTypes=Movie&sortBy=SortName&limit=200`, h).json();
  const withImage = ((img.Items) || []).find((i) => i.ImageTags && i.ImageTags.Primary);
  ctx.imageTag = withImage ? withImage.ImageTags.Primary : '0';
  const created = http.post(`${base}/Playlists`, JSON.stringify({ Name: 'bench-playlist', UserId: ctx.userId, Ids: ctx.itemId ? [ctx.itemId] : [] }), h);
  ctx.playlistId = created.status < 300 ? (created.json().Id || '') : '';
  return ctx;
}
