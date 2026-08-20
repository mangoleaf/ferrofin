#!/usr/bin/env python3
"""The benchmark endpoint table — the single source the whole suite consumes.

Ported 1:1 from the retired ``bench-lib.js`` ``ENDPOINTS`` array (the JS load
scripts are gone; ``suite/gen-registry.py`` now *imports* this module instead of
line-parsing JavaScript, so the table is plain data with no format convention
to keep).

Each entry:
    name      permanent trend key (mirrored by suite/registry.json variant ids)
    path      URL path+query as a format string over the run context — fields:
              {userId} {itemId} {imageItemId} {writeItemId} {seriesId}
              {playlistId} {taskId} {imageTag} {genreName} {studioName}
              {personName}  (the by-name fields arrive URL-quoted)
    method    default "GET"
    ok        expected status (200 unless stated — 204 for playstate writes);
              only responses with this status enter the latency distribution
    auth      False ⇒ no Authorization header (public endpoints, login)
    body      JSON body template for writes — string leaves are format strings
              over the same context (see benchlib.render_body)
    scenario  "login" ⇒ runs in its own window after the main legs drain
              (PBKDF2 saturates CPU and each login invalidates the server-side
              auth cache; a per-request DeviceId is generated as target data —
              reusing the main bench DeviceId would revoke the measurement
              token)

Fairness rules preserved from the JS (see suite/perf/README.md "Write rows"):
state writes target {writeItemId} (LAST movie by SortName), never the read
rows' {itemId}; bodies are fixed and state-preserving.
"""

# The body item_playbackinfo_post sends: jellyfin-web's shape with its default
# 120 Mbps streaming cap — a realistic client profile, small enough to keep the
# row about the server.
PLAYBACK_INFO_BODY = {
    "MaxStreamingBitrate": 120000000,
    "AutoOpenLiveStream": False,
    "DeviceProfile": {
        "MaxStreamingBitrate": 120000000,
        "DirectPlayProfiles": [
            {"Container": "mp4,m4v", "Type": "Video", "VideoCodec": "h264,hevc", "AudioCodec": "aac,mp3,opus"},
            {"Container": "mkv", "Type": "Video", "VideoCodec": "h264,hevc", "AudioCodec": "aac,mp3,opus"},
        ],
        "TranscodingProfiles": [
            {"Container": "ts", "Type": "Video", "VideoCodec": "h264", "AudioCodec": "aac",
             "Context": "Streaming", "Protocol": "hls", "MinSegments": 1},
        ],
        "CodecProfiles": [],
        "SubtitleProfiles": [{"Format": "vtt", "Method": "External"}],
    },
}


def _e(name, path, method="GET", ok=200, auth=True, body=None, scenario=None):
    return {"name": name, "path": path, "method": method, "ok": ok, "auth": auth,
            "body": body, "scenario": scenario}


ENDPOINTS = [
    # Framework floor — near-zero work, isolates routing/serialization overhead.
    _e("info_public", "/System/Info/Public", auth=False),
    _e("system_info", "/System/Info"),
    _e("system_endpoint", "/System/Endpoint"),
    _e("localization_cultures", "/Localization/Cultures"),
    _e("user_me", "/Users/Me"),
    _e("sessions", "/Sessions"),
    _e("scheduled_tasks", "/ScheduledTasks"),
    _e("plugins", "/Plugins"),
    _e("media_folders", "/Library/MediaFolders"),
    _e("virtual_folders", "/Library/VirtualFolders"),
    # Home-screen assembly.
    _e("user_views", "/UserViews?userId={userId}"),
    _e("items_latest", "/Items/Latest?userId={userId}&limit=20"),
    _e("items_resume", "/UserItems/Resume?userId={userId}&limit=12"),
    _e("nextup", "/Shows/NextUp?userId={userId}&limit=24"),
    _e("upcoming", "/Shows/Upcoming?userId={userId}&limit=24"),
    # Library query + DTO hot path — the query planner + PascalCase serialization under load.
    _e("items_sortname", "/Items?userId={userId}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=SortName"),
    _e("items_datesort", "/Items?userId={userId}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=DateCreated&sortOrder=Descending"),
    _e("items_episodes", "/Items?userId={userId}&recursive=true&includeItemTypes=Episode&limit=50&sortBy=SortName"),
    _e("items_series", "/Items?userId={userId}&recursive=true&includeItemTypes=Series&limit=50&sortBy=SortName"),
    _e("items_mixed", "/Items?userId={userId}&recursive=true&limit=100&sortBy=SortName"),
    # Faceted browse — GROUP BY / DISTINCT paths over the item set.
    _e("genres", "/Genres?userId={userId}"),
    _e("persons", "/Persons?userId={userId}&limit=100"),
    _e("studios", "/Studios?userId={userId}"),
    _e("years", "/Years?userId={userId}"),
    _e("filters", "/Items/Filters?userId={userId}&includeItemTypes=Movie"),
    _e("search_hints", "/Search/Hints?userId={userId}&searchTerm=a&limit=20"),
    # Single-item detail + related.
    _e("item_detail", "/Items/{itemId}?userId={userId}"),
    _e("item_ancestors", "/Items/{itemId}/Ancestors?userId={userId}"),
    _e("item_images", "/Items/{itemId}/Images"),
    _e("item_similar", "/Items/{itemId}/Similar?userId={userId}&limit=12"),

    # ── Broader read surface ──────────────────────────────────────────────────
    # Every endpoint below is a GET the parity ledger certifies both servers
    # answer 200, needing only userId/itemId. Grouped by subsystem.

    # System / diagnostics / meta.
    _e("system_ping", "/System/Ping"),
    _e("system_storage", "/System/Info/Storage"),
    _e("system_config", "/System/Configuration"),
    _e("metadata_options", "/System/Configuration/MetadataOptions/Default"),
    _e("activity_log", "/System/ActivityLog/Entries"),
    _e("system_logs", "/System/Logs"),
    _e("utc_time", "/GetUtcTime"),

    # Localization / branding / auth dictionaries (cheap, mostly-static payloads).
    _e("loc_countries", "/Localization/Countries"),
    _e("loc_options", "/Localization/Options"),
    _e("loc_parental", "/Localization/ParentalRatings"),
    _e("branding", "/Branding/Configuration"),
    _e("auth_providers", "/Auth/Providers"),
    _e("auth_pw_providers", "/Auth/PasswordResetProviders"),
    _e("auth_keys", "/Auth/Keys"),
    _e("quick_connect", "/QuickConnect/Enabled"),

    # Users / devices / view grouping.
    _e("users_all", "/Users"),
    _e("users_public", "/Users/Public"),
    _e("devices", "/Devices"),
    _e("grouping_options", "/UserViews/GroupingOptions"),

    # Library configuration + faceted browse.
    _e("physical_paths", "/Library/PhysicalPaths"),
    _e("available_options", "/Libraries/AvailableOptions"),
    _e("items_counts", "/Items/Counts?userId={userId}"),
    _e("items_filters2", "/Items/Filters2?userId={userId}&includeItemTypes=Movie"),
    _e("suggestions", "/Items/Suggestions?userId={userId}&limit=20"),
    _e("artists", "/Artists?userId={userId}"),
    _e("album_artists", "/Artists/AlbumArtists?userId={userId}"),
    _e("music_genres", "/MusicGenres?userId={userId}"),
    _e("movie_recommendations", "/Movies/Recommendations?userId={userId}&categoryLimit=6&itemLimit=8"),

    # Query-planner variety — same /Items path, different sort/filter/paging shapes.
    _e("items_random", "/Items?userId={userId}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=Random"),
    _e("items_rating", "/Items?userId={userId}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=CommunityRating&sortOrder=Descending"),
    _e("items_paged", "/Items?userId={userId}&recursive=true&includeItemTypes=Movie&startIndex=100&limit=50&sortBy=SortName"),
    _e("items_boxset", "/Items?userId={userId}&recursive=true&includeItemTypes=BoxSet&limit=50"),
    _e("items_favorite", "/Items?userId={userId}&recursive=true&includeItemTypes=Movie&limit=50&filters=IsFavorite"),

    # Channels / SyncPlay / Live TV subsystem handlers (return empty defaults,
    # but exercise the real route + serializer on both servers).
    _e("channels", "/Channels"),
    _e("syncplay_list", "/SyncPlay/List"),
    _e("livetv_info", "/LiveTv/Info"),
    _e("livetv_channels", "/LiveTv/Channels"),
    _e("livetv_programs", "/LiveTv/Programs"),
    _e("livetv_recordings", "/LiveTv/Recordings"),
    _e("livetv_series_timers", "/LiveTv/SeriesTimers"),
    _e("livetv_timers", "/LiveTv/Timers"),

    # Item detail sub-resources (all keyed on the picked movie itemId).
    _e("item_playbackinfo", "/Items/{itemId}/PlaybackInfo?userId={userId}"),
    _e("item_external_ids", "/Items/{itemId}/ExternalIdInfos"),
    _e("item_critic_reviews", "/Items/{itemId}/CriticReviews"),
    _e("item_intros", "/Items/{itemId}/Intros?userId={userId}"),
    _e("item_special_features", "/Items/{itemId}/SpecialFeatures?userId={userId}"),
    _e("item_local_trailers", "/Items/{itemId}/LocalTrailers?userId={userId}"),
    _e("item_theme_media", "/Items/{itemId}/ThemeMedia?userId={userId}"),
    _e("item_instant_mix", "/Items/{itemId}/InstantMix?userId={userId}&limit=20"),
    _e("item_userdata", "/UserItems/{itemId}/UserData?userId={userId}"),
    _e("item_similar_movie", "/Movies/{itemId}/Similar?userId={userId}&limit=12"),
    _e("media_segments", "/MediaSegments/{itemId}"),

    # Image serve + resize (ferrofin-drawing). Best-effort: N/A if no local poster is discovered.
    _e("image_primary", "/Items/{imageItemId}/Images/Primary?fillHeight=400&fillWidth=400"),

    # ── Expanded surface (2026-08: 43/412 benched ops was ~10%) ──────────────
    # Every entry is a ledger-certified GET whose params resolve from the
    # enriched context (benchlib.enrich_context). Grouped by subsystem.

    # TV browse — the Shows tab's two heavy queries.
    _e("shows_episodes", "/Shows/{seriesId}/Episodes?userId={userId}&limit=50"),
    _e("shows_seasons", "/Shows/{seriesId}/Seasons?userId={userId}"),

    # Item detail sub-resources (continued).
    _e("item_theme_songs", "/Items/{itemId}/ThemeSongs?userId={userId}"),
    _e("item_theme_videos", "/Items/{itemId}/ThemeVideos?userId={userId}"),
    _e("video_additional_parts", "/Videos/{itemId}/AdditionalParts?userId={userId}"),

    # Image routes: indexed and the fully-parametrized (path-baked transform) form.
    _e("item_image_indexed", "/Items/{imageItemId}/Images/Primary/0"),
    _e("item_image_parametrized", "/Items/{imageItemId}/Images/Primary/0/{imageTag}/webp/300/450/0/0"),

    # Playlists (created once by enrich_context).
    _e("playlist_detail", "/Playlists/{playlistId}"),
    _e("playlist_items", "/Playlists/{playlistId}/Items?userId={userId}"),
    _e("playlist_users", "/Playlists/{playlistId}/Users"),

    # Branding / fonts / startup / environment / system.
    _e("branding_css", "/Branding/Css"),
    _e("branding_css_ext", "/Branding/Css.css"),
    _e("fallback_fonts", "/FallbackFont/Fonts"),
    _e("startup_config", "/Startup/Configuration"),
    _e("startup_first_user", "/Startup/FirstUser"),
    _e("startup_user", "/Startup/User"),
    _e("env_default_browser", "/Environment/DefaultDirectoryBrowser"),
    _e("env_network_shares", "/Environment/NetworkShares"),
    _e("system_config_key", "/System/Configuration/encoding"),
    _e("backup_list", "/Backup"),
    _e("tmdb_config", "/Tmdb/ClientConfiguration"),

    # Devices / users / tasks (ids from the enriched context).
    _e("devices_info", "/Devices/Info?id=bench"),
    _e("user_by_id", "/Users/{userId}"),
    _e("scheduled_task_detail", "/ScheduledTasks/{taskId}"),

    # Channels + Live TV read surface (empty-but-real on the bench library).
    _e("channels_features", "/Channels/Features"),
    _e("channels_latest", "/Channels/Items/Latest?userId={userId}"),
    _e("livetv_programs_recommended", "/LiveTv/Programs/Recommended?userId={userId}"),
    _e("livetv_recording_folders", "/LiveTv/Recordings/Folders?userId={userId}"),
    _e("livetv_recordings_series", "/LiveTv/Recordings/Series?userId={userId}"),
    _e("livetv_recording_groups", "/LiveTv/Recordings/Groups"),
    _e("livetv_listing_default", "/LiveTv/ListingProviders/Default"),

    # ── Coverage push (2026-08: 109/412 contract operations were benched) ────
    # Same admission rule as the block above: a stateless GET whose params
    # resolve from benchlib.enrich_context and that both servers answer 2xx on
    # the bench fixture. Everything NOT here is listed with a reason in
    # suite/coverage.py — that file is the gate that keeps the split honest.
    #
    # New rows carry no rates.json entry yet, so compare.py drives them at the
    # flat default and records source="flat-default"; the next
    # `--calibrate-rates` fills them in. They are NOT in the 11 perf-gate
    # sentinels, so the mandatory 5-minute gate is unchanged in length.

    # By-name facet detail — the second half of the /Genres, /Studios, /Persons
    # browse pair (the list rows above, the single-entity lookup here).
    _e("genre_detail", "/Genres/{genreName}?userId={userId}"),
    _e("studio_detail", "/Studios/{studioName}?userId={userId}"),
    _e("person_detail", "/Persons/{personName}?userId={userId}"),

    # Similar/related shapes the movie rows above don't reach.
    _e("shows_similar", "/Shows/{seriesId}/Similar?userId={userId}&limit=12"),
    _e("trailers", "/Trailers?userId={userId}&limit=50"),
    _e("trailers_similar", "/Trailers/{itemId}/Similar?userId={userId}&limit=12"),

    # Playlist sub-resources (the bench playlist enrich_context resolves).
    _e("playlist_user", "/Playlists/{playlistId}/Users/{userId}"),
    _e("playlist_instant_mix", "/Playlists/{playlistId}/InstantMix?userId={userId}&limit=20"),

    # Item detail sub-resources — the metadata-editor blob (parental ratings +
    # culture tables) and the local provider list. Neither touches the network.
    _e("item_metadata_editor", "/Items/{itemId}/MetadataEditor"),
    _e("item_remote_image_providers", "/Items/{itemId}/RemoteImages/Providers"),

    # Per-user display preferences — what jellyfin-web reads on every page load.
    _e("display_preferences", "/DisplayPreferences/usersettings?userId={userId}&client=emby"),

    # Dashboard / environment reads (no directory listing: its answer depends on
    # the container image — see coverage.py "host-fs").
    _e("env_drives", "/Environment/Drives"),
    _e("env_parent_path", "/Environment/ParentPath?path=%2Ftmp%2Fx"),
    _e("repositories", "/Repositories"),
    _e("config_pages", "/web/ConfigurationPages"),

    # Live TV configuration surface (empty-but-real without a tuner, like the
    # livetv_* rows above).
    _e("livetv_guide_info", "/LiveTv/GuideInfo"),
    _e("livetv_channel_mapping_options", "/LiveTv/ChannelMappingOptions"),
    _e("livetv_timer_defaults", "/LiveTv/Timers/Defaults"),
    _e("livetv_tuner_types", "/LiveTv/TunerHosts/Types"),
    _e("livetv_listing_lineups", "/LiveTv/ListingProviders/Lineups"),
    # The POST twin of livetv_programs: a query, not a mutation — the body is
    # the filter, so this row is as state-preserving as the GET.
    _e("livetv_programs_post", "/LiveTv/Programs", method="POST",
       body={"UserId": "{userId}", "Limit": 20}),

    # ── Write surface (tiers 1–2: read-shaped POSTs + idempotent upserts).
    # Rules that keep write rows honest (see README "Write rows"): state writes
    # target {writeItemId} (LAST movie by SortName) so write traffic can't
    # drift a read row's body; bodies are fixed and state-preserving; merge.py
    # exempts non-GET ops from the body-shape fingerprint (their honesty gate
    # is the parity write JOURNEY + 100% expected-status).
    _e("item_playbackinfo_post", "/Items/{itemId}/PlaybackInfo?userId={userId}", method="POST", body=PLAYBACK_INFO_BODY),
    _e("playstate_progress", "/Sessions/Playing/Progress", method="POST", ok=204,
       body={"ItemId": "{writeItemId}", "PositionTicks": 0, "IsPaused": False, "PlayMethod": "DirectPlay"}),
    _e("item_userdata_post", "/UserItems/{writeItemId}/UserData?userId={userId}", method="POST",
       body={"PlaybackPositionTicks": 0, "Played": False, "IsFavorite": False}),
    # Login storm — its OWN open-loop window after the main legs drain.
    _e("auth_login", "/Users/AuthenticateByName", method="POST", auth=False, scenario="login",
       body={"Username": "{username}", "Pw": "{password}"}),
]

BY_NAME = {e["name"]: e for e in ENDPOINTS}

assert len(BY_NAME) == len(ENDPOINTS), "duplicate endpoint name"
