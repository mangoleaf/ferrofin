# Changelog

All notable changes to Ferrofin are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com) and
[Conventional Commits](https://www.conventionalcommits.org); versions follow
[Semantic Versioning](https://semver.org).

## [Unreleased]

### Upgrade Notes
- Per-library metadata/image fetcher selections are now enforced during the
  scan, and five built-in fetchers are newly named in library options
  (TheTVDB, FanArt, MusicBrainz, TheAudioDB, Embedded Image Extractor). A
  library whose options were last saved by an older Ferrofin will not have
  these in its saved lists, so they stop running for that library after the
  upgrade — open each library's settings and Save once to re-enable them.
  Libraries migrated from a Jellyfin database, and libraries created after
  the upgrade, are unaffected.

### Bug Fixes
- Report CanUninstall=true so the dashboard shows the enable/disable toggle
- Correct transcode.js fixture path after the suite/ reorg
- Give episodes their cast — merge series regulars + surface roles
- Detect BOM-less UTF-16 + restore the -sub_charenc hint

### Features
- Accumulate benchmark reruns per SHA instead of overwriting
- Prune items whose files were deleted from disk
- Push UserDataChanged to the user's other devices
- Send the WebSocket pushes Jellyfin clients rely on
- Live filesystem watching of library roots (inotify via notify)
- Debounce filesystem changes behind LibraryMonitorDelay
- Path-scoped ingest — resolve just the changed files
- Store GUIDs and datetimes in Jellyfin's exact text formats
- Pin the schema to Jellyfin 10.11.8 — migration 0007 + code convergence
- BaseItems.Data JSON is the playlist/collection source of truth
- Adopt an existing Jellyfin 10.11.8 database in place

### Testing
- Schema-conformance gate + the drop-in round-trip test

## [0.16.0] - 2026-08-08

### Bug Fixes
- Scope per-library refresh to that library only

### Features
- Fanart.tv artwork + scan-time provider ids + audio tags
- Materialize MusicArtist items from album-artist tags
- Add MusicBrainz client (ws/2, throttled)
- Resolve MusicBrainz ids for music items (post-scan pass)
- AudioDb + fanart music leg + provider client tests

### Testing
- Cover the absent-plugin 404 branch

## [0.15.0] - 2026-08-07

### Bug Fixes
- Cap transcoded audio channels per encoder

### Features
- Add Studio Images and TheTVDB metadata providers

## [0.14.1] - 2026-08-07

### Bug Fixes
- Stop rendering subtitles twice (burn-in + client overlay)

## [0.14.0] - 2026-08-07

### Bug Fixes
- Derive SortName from the name for by-name items

### Features
- Build movie recommendations from watch state
- Port MergeVersions as a full extension (upstream 12.0)

### Refactor
- Consolidate benchmark/ + parity/ into the one suite/ folder
- Move raw SQL behind a repository boundary

## [0.13.0] - 2026-08-06

### Bug Fixes
- Apply NFO <title> as the authoritative item name
- Match Jellyfin's default + CustomPrefs serialization
- Fall back to metadata folder when media is read-only
- Stamp image date_modified from file mtime, not scan time
- Match Jellyfin's by-name item shape (Genre/Studio/Person)
- Count a Person's credited items via the People map
- Stop the transcode kill/restart storm when the source keeps failing

### Features
- Lossy WebP output via libwebp (Skia parity)
- Port the weighted similarity scorer (item_similar*)

### Testing
- Dedupe raw test SQL back under the sql_boundary ceilings

### Skill
- Add final helm-render compatibility review

### Style
- If/else over single-pattern match (clippy)
- Rustfmt the WebP encoder + collage-test formatting

## [0.12.1] - 2026-08-06

### Bug Fixes
- Hand off k6's provisioned token for post-load captures
- Error panels read 0 instead of "no data" when there are no errors

### Performance
- Run image encode/resize on spawn_blocking
- Bound concurrent image encodes to core count

### Testing
- Guard get_playlist_items link order

### Bench
- Clean release record v0.12.0 / 553252f (parity + perf)

### Chart
- Add de-identified values.example.yaml + maintenance skill

## [0.12.0] - 2026-08-05

### Features
- Instrument scheduled-task runs with a root span
- Instrument library scans with a trigger-tagged root span
- Log DB pool open and migration head (Step 2)
- Make library-scan progress cadence configurable
- Add the auth security-audit trail
- Instrument transcode jobs + log ffmpeg's exit
- Span the intro-skipper run + websocket session
- Provider fetch failures + published-URL resolution

## [0.11.0] - 2026-08-05

### Documentation
- Add RULES_LOGGING pointer to CLAUDE.md

### Features
- Add golden-signals + deep-dive Grafana dashboards
- Retention, non-blocking writer, panic hook, shutdown reason

### Testing
- Assert JSON log lines carry the span trace_id

## [0.10.0] - 2026-08-05

### Bug Fixes
- Report progress + log per-season so a stalled run is visible

### Features
- OTLP trace export with log↔trace correlation
- Redact secret strings behind a Secret newtype

## [0.9.0] - 2026-08-05

### Features
- Add OpenTelemetry-backed Prometheus /metrics endpoint
- Add FERROFIN_ENABLE_METRICS bootstrap override

### Miscellaneous
- Add run-benchmark skill
- Add missing crate dependency
- Add secrecy crate dependency to ferrofin-providers and ferrofin-model

### Performance
- Serve user DTOs from the auth cache instead of 2-3 DB round-trips per request
- Reuse a pre-minted token for post-load captures

### Refactor
- Give subsystem crates typed errors that convert into ServiceError

### Bench
- Extend the surface to write paths — 4 POST variants + write-row comparability
- Release suite record b41adc1 (parity + perf)

## [0.8.6] - 2026-08-04

### Bug Fixes
- Maintenance can no longer break live playback

### CI/CD
- Prebuild the ffmpeg runtime base so the service image stops re-running apt

### Bench
- Expand the surface 83 -> 114 variants; honesty check goes cross-server

### Suite
- First run on the expanded 114-variant surface

## [0.8.5] - 2026-08-04

### Bug Fixes
- Compute UserData.PlayedPercentage for in-progress leaf items

### Performance
- First real baseline + proven detector + absolute jitter floor
- Kill the per-request SQLite tax on the authenticated hot path
- Port Jellyfin's poster caching contract — fixes slow poster loads on TV clients
- Rebaseline on the auth+images+played-percentage tree

### Parity
- Refresh ledger from the plan-8 suite run; drop the stale pre-plan-7 seed

### Suite
- First real seed data on the settled tree (plan 8 step 6)
- Drop the stale cefe2f8 entry from runs.json too
- Carry HLS play-start TTFS into the merged record and viewer

### Viewer
- Carry the retired dashboards' visual design forward
- Run selection up under the title; footprint as comparison cards

## [0.8.4] - 2026-08-03

### CI/CD
- Best-effort perf-gate job — self-contained merge-base comparison

### Documentation
- Perf/parity remediation plan set (plans 1-7, as executed)
- Plan 8 — close out plan 3/4/6 deviations (single-agent)
- Accepted-deviations ledger; ignore __pycache__ and stray result logs

### Performance
- One projection path — single item is a batch of one
- Add Ferrofin-only p50/p95/p99 regression gate (plan 4)
- Unify parity + benchmark into one cross-referenced suite
- Split reader/writer pools; pool size becomes a config knob (auto = cores)
- One comparator, one baseline — suite/gate.py absorbs perf-gate.mjs

### Bench
- DB pool sweep harness + realistic-load phase D
- Finish the bring-up consolidation onto suite/lib.sh

### Viewer
- One dashboard — suite viewer absorbs :8123 and :8124

## [0.8.3] - 2026-08-03

### Bug Fixes
- Install fpcalc (libchromaprint-tools) in the benchmark image

### Performance
- Stop materializing id sets and binding giant INs
- Push pagination, name filters, and total-count into SQL
- Chunked guide inserts + SQL boundary ratchet test
- Skip always-zero played/child counts for by-name rows

## [0.8.2] - 2026-08-03

### Bug Fixes
- -noaccurate_seek on seeked video-copy streams (audio led video)

### Performance
- Batch folder played/total counts into two grouped joins

## [0.8.1] - 2026-08-03

### Bug Fixes
- Don't wipe resume point on a positionless stop report
- Seed the fMP4 init transcode at the resume offset

## [0.8.0] - 2026-08-03

### Features
- Switch to jellyfin-ffmpeg7 for NVENC/tonemapx support

### Testing
- Layer-3 binary/asset differential (images, fonts, css)

## [0.7.1] - 2026-08-03

### Bug Fixes
- Accept session commands like Jellyfin (no remote-control gate; optional GeneralCommand fields)
- Serve GET /System/Configuration/Branding (was 405)
- Run QuickConnect under its own DeviceId (don't clobber harness session)

### Testing
- Session remote-control journey (12 ops)
- Admin/config/device/auth write journeys + device reads
- Subtitles upload, GET /Devices/Options, MergeVersions controller probe
- QuickConnect handshake journey — untested reaches 0
- Make DELETE /Items bulk check status-based (Jellyfin deletes async)

### Parity
- Classify not-testable-via-differential ops (destructive/wizard/host-fs/Jellyfin-bug)
- Classify MergeVersions extension, Items/Root, Years/{year}, music by-name

## [0.7.0] - 2026-08-03

### Bug Fixes
- Populate SeriesName/SeasonName + folder UnplayedItemCount

### Features
- Read local Kodi/XBMC NFO sidecars during the library scan

### Testing
- Legacy playstate, password, vfolder rename journeys; classify LiveStreams
- VirtualFolders CRUD journey + resolved-param GET reads
- Ping/refresh/content-type journeys + Playlists GET read-backs

### Parity
- Classify the not-testable-via-body-diff surface (untested 259->83)

## [0.6.2] - 2026-08-02

### Bug Fixes
- Surface NowPlayingItem + PlayState from playback reports
- Include merged alternate versions in GetStaticMediaSources
- Install fpcalc (libchromaprint-tools) for intro skipper

### Miscellaneous
- Refresh ledger — session NowPlayingItem/PlayState fix lands (deep-verified 110)
- Refresh ledger — MergeVersions/AlternateSources fix lands (deep-verified 115)

### Performance
- Cap default SQLite pool by cgroup CFS quota, not just affinity

### Testing
- Write journeys for item-delete, session capabilities, path validation, video merge

## [0.6.1] - 2026-08-02

### Bug Fixes
- Stop emitting -maxrate/-bufsize (and -r) twice
- Audio downmix parity — libfdk_aac preference + volume=2 boost

### Documentation
- CLAUDE.md toolchain pin 1.97.0 -> 1.97.1 (match rust-toolchain.toml)

### Miscellaneous
- Classify playstate-progress + playlist-share-delete as methodology (not Ferrofin bugs)

### Performance
- Use jellyfin-ffmpeg's tonemapx for software HDR tonemap

### Testing
- Expand write journeys — API keys, UserData, DisplayPreferences, task triggers, device options, playlist move
- Write journeys for playstate, capabilities, user/system config, playlist share
- Reorganize integration tests by domain, not batch

## [0.6.0] - 2026-08-02

### Bug Fixes
- Name the cause when media reads fail at playback
- Deleting a box-set/playlist must not delete its linked members (data loss)
- 6 real GET divergences — LiveTv defaults, server config, Users/Public, Items/Counts
- Vendor Jellyfin's localization data (countries, US ratings, UI options)
- LiveTv/Info service visibility+tuners, MetadataEditor ContentTypeOptions

### CI/CD
- Bump rust CI + build images to 1.97.1 (match rust-toolchain.toml)

### Documentation
- Round-2 triage verdicts — 12 real bugs in the 13 flagged GETs

### Features
- Deep-diff the whole GET surface in the sweep (untested 361->280)

### Miscellaneous
- Refresh ledger after data-loss fix — write journeys 19/19, deep-verified 89
- Refresh ledger after batch-3 fixes + classify residual instance noise
- Refresh ledger — localization vendoring lands, deep-verified 95
- Refresh ledger + classify item-edit (oracle) & MetadataEditor (ExternalIdInfos)

### Performance
- Default the SQLite pool to available parallelism

### Testing
- Expect ItemCount=0 in counts_group_by_kind
- Expect empty ContentTypeOptions for a plain item

### Bench
- Isolate phase runs in their own compose project + ports

### Parity
- Denylist per-instance fields + classify confident not-bugs (flagged 49->15)

## [0.5.3] - 2026-08-02

### Bug Fixes
- V-prefix docker image tags to match git tags

### Performance
- Prebuild jellyfin-web into a CI image; stop webpacking per build

### Testing
- Lock WI-6 — collection members visible via parentId browse

### Bench
- Don't let a server's scan crash abort the whole run
- Retry scan up to 3× so an intermittent Jellyfin OOM isn't fatal

### Parity
- Mark the not-bugs as accepted classifications (curated, highest precedence)

## [0.5.2] - 2026-08-02

### Bug Fixes
- Always emit ImageTags as {} instead of omitting it
- Early-triggerable release gate via a needs-a-manual-job pattern
- Extract startup banner from run() to satisfy too_many_lines
- Batch child counts in upstream shape with LinkedChildren precedence
- Always emit ImageBlurHashes as {} (BaseItemDto null-field sweep)

### Miscellaneous
- Split the startup banner into grouped lines

### Bench
- Fix endpoint loop — name escaping, stdin, abort hardening

## [0.5.1] - 2026-08-02

### Refactor
- Extract version generation to scripts/version.sh

## [0.5.0] - 2026-08-02

### Bug Fixes
- Batch 1 — image cache tags, Filters2 shape, playlist id round-trip, RefreshStatus
- Stamp MessageId on every outbound WebSocket message
- Batch 2 — MediaSource fields, drop IsOriginal, CanDelete, BoxSet browse, Playlists folder
- Release by tagging the commit, not committing to main
- Keep create-release manual; gate it on lint+coverage via needs

### Build
- Default the reported version to git describe, not a stale constant

### CI/CD
- Publish describe-style dev images for build-worthy commits
- Gate :latest solely on a final release tag at the push site
- Cancel the redundant dev image build when a release is cut
- Allow triggering create-release early, auto-cancel if tests fail

### Documentation
- Fix env example to FERROFIN_LOG (the var Ferrofin reads)
- Triage roadmap + per-op verdicts from the parity-triage workflow

### Features
- Chart-managed env ConfigMap injected into Ferrofin

### Miscellaneous
- Refresh ledger after batch 1+2 fixes; denylist ETag (instance hash)

### Bench
- Isolated open-model per-endpoint harness with cgroup CPU cost
- Auto-detect new runs, auto-refresh, and notify
- Per-endpoint saturation sweep for max sustainable RPS
- Mixed contention run + cgroup memory.peak footprint

## [0.4.1] - 2026-08-02

### Bug Fixes
- Write server logs to disk and stop dropping client uploads
- Stop skipping the test stage on non-image commits

### CI/CD
- Add prebuilt Rust CI base image with nextest/llvm-cov baked in

## [0.4.0] - 2026-08-02

### Bug Fixes
- Report image file Size in GetItemImageInfos (parity)
- Serialize MediaStream computed properties + map NalLengthSize (parity)
- Advertise ProductName as "Jellyfin Server" for client compatibility

### CI/CD
- Build image only on release tags; gate create-release on lint+coverage
- Run tests with cargo nextest (keep cargo test --doc for doctests)

### Features
- Layer-2 read depth with id-correlation (Phase 1 task 6)
- Persist per-field read diffs; image Size fix reflected in ledger

### Miscellaneous
- Refresh ledger after media-info fix (Items/{id} missing 170->70)

### Performance
- Assemble list media sources from prefetched streams; single-pass Filters2 languages
- Page-batch cast/crew people and their images in list DTO projection
- Page-batch studio/genre/artist value-id resolution in list DTO projection
- Page-batch chapters and trickplay in list DTO projection

## [0.3.0] - 2026-08-01

### Bug Fixes
- Report the real release version and let CI own the bump
- Run create-release on alpine/git, derive bump in shell
- Serde-default TypeOptions so jellyfin-web VirtualFolders bodies deserialize
- Serde-default remaining client-body DTOs (TypeOptions-422 audit)
- Anchor breaking detection to the CC footer; cap 0.x at minor
- Derive the semver bump from the commit subject only

### Features
- Per-operation parity ledger + Layer-1 sweep + Layer-2 write journeys

### Performance
- Batch N+1 query loops in by-name, item-detail, and user DTO paths
- Derive recommendation DtoOptions from fields; page-batch DTO media streams + provider ids

### Testing
- Add multi-version results viewer and expand endpoint coverage
- Send realistic jellyfin-web library body so the sweep catches serde-422s

## [0.2.4] - 2026-08-01

### Bug Fixes
- Emit ChildCount on latest items; denylist per-session instance noise
- Seed admin preserves per-user policy defaults instead of blanking them
- Default new users to Jellyfin's HidePlayedInLatest + CastReceiverId
- Persist EnableContentDeletion + EnableRemoteControlOfOtherUsers permissions
- Report IsActive=true for a controller-less session
- Emit session Capabilities/PlayState/queues; denylist task run-times
- Ship Jellyfin's two built-in cast receiver applications
- Compute SortName during the scan (Jellyfin CreateSortName)

### Features
- Response-body parity harness + first fixes (Fields, folder-movie naming, blurhash)
- Populate DTO field defaults + honour fields on /Items/Latest
- Compute image dimensions + blurhash during the scan
- Serve the full ISO-639 culture list
- Bundle pinned jellyfin-web client at /usr/share/ferrofin/web

## [0.2.3] - 2026-08-01

### Bug Fixes
- Stop create-release SIGPIPE on tag lookup, set tagger identity

### CI/CD
- Add manual create-release job that cuts the tag
- Replace GitLab cache with sccache-on-Garage

## [0.2.2] - 2026-07-31

### CI/CD
- Make the git tag the single source of truth for versioning

## [0.2.1] - 2026-07-31

### Miscellaneous
- Cargo fmt

## [0.2.0] - 2026-07-31

### Documentation
- Use registry.mangoleafstudios.com host

### Features
- Official Helm chart + OCI publish on release

### Miscellaneous
- Bump rust toolchain to 1.97.1

### Release
- V0.2.0

## [0.1.0] - 2026-07-30

### Bug Fixes
- Port 5 wire DTOs documented but never defined in Wave 1
- Remove SPA fallback that black-screened the web client
- Report Jellyfin API version 10.11.8, not the crate version
- First-time-setup auth for the localization endpoints
- Seed a passwordless admin so the setup wizard works
- First-time-setup auth for library-structure + environment
- Case-insensitive API routing (match Jellyfin/ASP.NET)
- First-time-setup auth for /Libraries/AvailableOptions
- Build auth-result User DTO via get_user_dto; persist server id
- Populate SystemInfo.OperatingSystem (was null)
- Default ValidatePathDto.validate_writable (was 422)
- Case-fold query-param keys (Jellyfin is case-insensitive)
- Resolve LibraryOptions POST by CollectionFolder Id (was 404)
- Self-heal missing CollectionFolder row (scan FK 500)
- Quote-aware ffmpeg/ffprobe arg splitting
- Accept jellyfin-web bodies that strict serde rejected (422s)
- Transcode incompatible audio (no-audio on playback)
- Resolve codec preference list + number seek segments
- Evict stale transcode on seek so scrubbing doesn't corrupt
- Align seek-segment timestamps to the playlist (-output_ts_offset)
- Report each library's CollectionType so TV renders as shows
- Deliver HEVC as fMP4 tagged hvc1 so HDR HEVC plays
- Force fMP4 HLS container for HEVC/AV1 (was mkv 404)
- Downmix HLS transcode audio to the profile channel cap
- Transcode to h264, not the client's first codec preference
- Render season/episode artwork (default DtoOptions.image_types to all)
- Legacy favorite/played/rating routes + aired episode order
- DELETE /Items/{id} no longer 500s on playlists/collection members
- Honor StartItemId on /Shows/{seriesId}/Episodes
- Detect Dolby Vision so DV content transcodes for browsers
- Extract + persist embedded chapters for the playback timeline
- Honor fillWidth/fillHeight for unknown-size images (poster clipping)
- Populate MediaStream DisplayTitle (was 'Undefined')
- Write ItemValues for genres/studios/tags (fixes More Like This)
- Deliver + burn in selected subtitles (were never rendered)
- Write-first transactions to end SQLITE_BUSY_SNAPSHOT + AncestorIds 500
- Populate Episodes/Genres/Studios tabs + Suggestions
- Map series studios from broadcast networks, not production companies
- Report real disk free/used for every folder
- Wire real ffmpeg subtitle encoder (external VTT delivery)
- Materialize Person items + cast/crew artwork; async scan
- Apply PersonIds/Person filter so a person page lists their work
- Run intro-skipper analysis in the background
- Persist cleared like, fix Latest filters + media folders
- Make subtitle delivery, burn-in, and extraction actually work
- Serve the livetv named configuration instead of 501
- Accept repeated includeSegmentTypes query params
- Stamp HasSegments on media sources
- Fingerprint the full credits window, not fpcalc's 120s default
- Queue started tasks so Running state reflects the real run
- BENCH_SKIP_BUILD knob; stop RSS sampling before the transcode phase
- Stamp derived CleanName on save so items are searchable
- Merge repeated query params so array params don't 400

### CI/CD
- GitLab pipeline — semver-tagged docker images to the registry
- Build the release image with kaniko instead of docker-in-docker

### Documentation
- Add CLAUDE.md contributor & agent guide
- The client now fetches metadata, not just artwork

### Features
- Scaffold ferrofin workspace + Wave 0 port
- Port MediaBrowser.Model DTOs and enums
- Wave 2
- Port Jellyfin.Database schema + entities (sqlx + SQLite)
- Port MediaBrowser.Controller interfaces (the DI seam)
- Wave 5 impl crates
- Port core manager implementations (the workhorse)
- Port Jellyfin.Api — contract-complete axum layer
- Implement 236 endpoints for real (Wave 7b)
- Real HLS transcode pipeline + more endpoints
- Composition root — Ferrofin boots as a real server (Wave 8)
- Implement 31 of the 35 core-not-yet-wired routes
- Serve a static web client at /web
- Session WebSocket at /socket (fix "Connection Failure")
- Tier-1 compile-time plugin system + plugin-manager API
- Named config sections, Backup list, legacy Users/{id}/Items
- Create CollectionFolder item on library add (scan A1)
- Persist all POST /System/Configuration/{key} (was 501)
- Filesystem scan engine — movies → item rows (scan A2)
- Make library scan triggerable from the UI
- TV + music scan resolvers (Series/Season/Episode, Album/Audio)
- Probe media on scan (streams/duration/size) + PlaySessionId
- Path-scoped /Users/{userId}/Items/{Resume,Latest,itemId} routes
- RemoteAccess persist, real System/Endpoint, QuickConnect token
- Populate the plugin catalog from repository manifests
- Serve resized/converted images (was full-size original)
- Persist playlist shares (/Playlists/{id}/Users)
- Synchronized group playback over the session WebSocket
- Add SubtitleProvider seam for the provider registry
- OpenSubtitles provider + enriched search request
- NVENC hardware transcoding (full-GPU 4K/HDR)
- TV episode linkage + automatic TMDB artwork
- TMDB season posters + episode stills
- Persist artwork upload/delete + user avatar
- Read/write local .lrc/.elrc/.txt sidecars
- Implement drag-to-reorder (move_item)
- Ship the per-type MetadataOptions defaults
- TMDB remote metadata search + image browsing
- Honor insert position on POST /Playlists/{id}/Items
- M3U tuner + XMLTV guide parsers
- Schema for tuner hosts, listing providers, channels, programs
- Real DB-backed LiveTvManager + composition-root wiring
- Manager-backed LiveTv handlers + config/lookup routes
- Channel live-stream playback (Phase 8)
- DVR timer/series-timer/recording CRUD (Phase 9)
- Fetch full movie/series metadata from TMDB on scan
- Fetch person profile photos + trailers in details()
- Log playback to the dashboard activity feed
- Enrich cast/crew with TMDB biography, birthday, birthplace
- Rotten Tomatoes critic rating via OMDb
- Plan + ferrofin-chromaprint pure intro/credits math
- Intro Skipper — audio-fingerprint intro/credits detection
- Implement four hollow functional gaps end-to-end
- Dashboard settings page for plugins (Intro Skipper)
- Add Enable toggle to the Intro Skipper settings page
- Implement the last 5 Live TV routes; fix api-status audit
- Full Intro Skipper settings parity (9 tabs, ~70 config fields)
- Implement the full Intro Skipper plugin API surface
- Real metadata-provider registry for AvailableOptions
- Vendor real plugin web UIs; add the File Transformation extension
- Enforce ownership, shares, and open-access for real
- Real per-item metadata refresh, incl. seasons/episodes
- Working Ferrofin-vs-Jellyfin harness, first run green
- The full Jellyfin dashboard task set + a real trigger scheduler
- Real transcode TTFS — copy + forced-encode modes; fix 30s transcode start timeout
- Honor the negotiated bitrate/resolution caps — downscale, -maxrate, HDR tonemap
- Record playback decisions to PlaybackSessions (Track A)
- Bus-registered sockets are remote-controllable

### Miscellaneous
- Cleanup commited .rcg/ files
- Gitignore .rcg/
- Lockfile for ferrofin-networking dependency
- Fake MediaSourceManager impls + Cargo.lock for refresh_media_streams
- Refresh Cargo.lock for ferrofin-providers tokio/tracing deps
- Add hermitcodegraph server entry

### Performance
- Halve time-to-first-segment (temp_file + drop index+1 wait)
- Small SQLite pool + batched DTO relation loads
- 3s HLS segments + forced segment-boundary keyframes

### Style
- Rustfmt the person_ids filmography test
- Rustfmt the extensions registered_plugins builder


