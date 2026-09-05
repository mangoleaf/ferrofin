# Changelog

All notable changes to Ferrofin are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com) and
[Conventional Commits](https://www.conventionalcommits.org); versions follow
[Semantic Versioning](https://semver.org).

Upgrades needing a manual step or with a non-obvious behavior change are
called out in [docs/UPGRADING.md](docs/UPGRADING.md).

## [1.0.0] - 2026-09-05

### CI/CD
- Harden the workflows — SHA pins, least privilege, zizmor gate, CODEOWNERS

### Documentation
- Observability section — JSON logs, Prometheus metrics, OTLP traces
- Point plugin authors at ferrofin-plugin-template and its manifest
- Examples/wasm-hello is the WIT conformance fixture, the template is for authors
- Reset to the 1.0.0 baseline; drop the pre-release fetcher note
- Remove references to private plans, local skills and personal paths
- About update

### Miscellaneous
- Single vendored spec copy, drop the .port allowlist
- Small readme.md update

### Testing
- Stop racing statvfs against parallel tests in the fs-probe check

## [0.42.7] - 2026-09-05

### CI/CD
- Move to the Node 24 majors of the artifact and docker actions

## [0.42.6] - 2026-09-05

### Bug Fixes
- Bake the release tag into the binary's build version

### CI/CD
- Assert the image ships jellyfin-ffmpeg

## [0.42.5] - 2026-09-04

### Bug Fixes
- Compile on macOS and Windows targets

### CI/CD
- Check /health/live for the build tag, and boot the pushed image

## [0.42.4] - 2026-09-04

### CI/CD
- Build and attach server binaries to the GitHub Release
- Use actions/checkout@v5 (Node 20 runtime deprecation warning)
- Header says GitHub is canonical, not GitLab
- Grant actions:read so the green-CI gate can list runs

### Documentation
- Documentation updates

### Testing
- Cover the Database query helpers, backup and integrity paths

### Report
- Report the run-to-run range, stop ruling on it

## [0.42.3] - 2026-09-04

### Counts
- The whole-server played arm counts leaves, not user-data rows

### Report
- Make the headline tiles fit the window

## [0.42.2] - 2026-09-03

### Bench
- A third load level, and the guards a third level needs
- Name a run for the code it measured

### Counts
- Pin the played roll-up to drive from the page's parents

## [0.42.1] - 2026-09-03

### Bench
- A report you can read — markers in the cells, reasons in notes
- Report the precision the measurement actually has

### Latest
- Port v12's music branch — the newest albums, not the albums of the newest tracks

## [0.42.0] - 2026-09-03

### Bug Fixes
- Every measured endpoint counts in the headline
- A missing leg marks the run, it does not delete it
- Repair the render and stop hiding measured endpoints
- HEAD moving mid-run is not a stale build
- Repoint the Live TV foreign keys 0009 left dangling on adopted databases
- Name both causes when testdata bring-up cannot reach a server
- Footprint metrics measure real numbers — restart-based cold start, 100ms host-cgroup RSS sampling
- Scope to the user's libraries, port v12 DetermineNextEpisode — 1.29 s → 12 ms

### Features
- Keep both servers' responses beside every parity verdict
- Refuse to run if real media is writable from any suite container
- Pin a real Jellyfin snapshot as the suite's test data
- Both servers run on seeded copies of the pinned snapshot
- Pinned backup + a read-only streams stage — the two media rules
- Three-percentile speedups; verification depth leaves the perf headline

### Miscellaneous
- Regenerated parity results and the v0.40.0-82 run record
- Record the v0.41.0-3 run (130 measured, 1.94x, 1 leg missing)
- Record the v0.41.0-6 run (136 measured, 1.795x, no missing legs)
- Keep the code-graph index out of the docker build context
- First snapshot-corpus record — recalibrated rates, run 240db12, acked shape baseline

### Api
- The parity record lives beside the route table
- GET /Items and /UserItems/Resume take v12's DtoOptions and query inputs

### Bench
- Designed test data — generator, Jellyfin seeding, build
- The comparison run, its instruments and the report
- Report interference as p95 / max, not max alone
- The comparison viewer — report.py --serve
- Benchmark core Ferrofin — every plugin disabled, no WASM
- Shape records items returned and TotalRecordCount separately

### Dto
- V12 parity for the small DTO fields (P1.1–P1.6, P1.9)
- Port v12's inherited parent images and Series.Status

## [0.41.0] - 2026-09-01

### Bug Fixes
- Align the lab data dirs and close the folder-DTO divergences
- Pick the C# provider per kind, and 404 a seed that is not there
- Order the flat providers Random, and stop calling a property probe deep-verified
- Port the probe DTO, by-name and library-options divergences batch A3 found
- Port image format negotiation, the refresh fetcher gate, and the music/search sort keys batch B2 found
- Port the ?format= query binding arm and stop the asset layer borrowing the body-diff headline
- Make every ledger row declare how it was verified, and stop defaulting into the headline
- Restore merge.py's body-diff honesty gate, which the stamping commit dropped
- Stop the deep-verified headline being borrowed by a nested empty envelope, a dashboard, or an unenforced rule
- Save where Jellyfin saves, parse what Jellyfin parses
- Port the real LrcParser decoder, and gate the writes on LyricManagement
- Make tuner and listings-provider administration do the work
- Queue the guide refresh instead of blocking on it, and stop a side-path note standing in for a verdict
- Port the AggregateFolder root and its virtual-children concat
- Port the /Items user-root branch, and make array order diffable
- One repository store, a real package catalogue, no orphan views
- Bind assemblyGuid the way ASP.NET binds a Guid?
- Bind a blank assemblyGuid the way ASP.NET does
- Rename the ferrofin volume after its mount point moved
- Port the Years/InstantMix/dashboard-page behaviours the "out-of-scope" notes were hiding
- Close the cross-user read, and port the plugin surface the "no shared id" notes were hiding
- A non-admin could act as any other user through ?userId=
- Close the last GetUserId site, and bind an empty guid the way ASP.NET does
- Four "host-specific" rows were false labels hiding real bugs
- The task Id is portable, and the ffmpeg logs belong in the log dir
- The probe DTO tracks upstream master's nullability again
- Correct the record M1 wrote, not just the code
- Parent the playlists folder, and port the id/image lineups to 10.11.8
- Teach every stored-kind switch the real PlaylistsFolder name
- Derive channel/programme ids the way Jellyfin does, and bound the guide
- Port the whole UserPermissionRequirement policy, not just its handler
- Gate remote search on the library's downloaders, and stop typing artwork from the URL
- Make the Identify Apply row say what it actually measured
- A bare empty list is untested, not empty-corpus
- The server mints a series timer's id, and the timer it schedules
- A hand-cancelled showing stays cancelled, and cancelling a series takes its completed children
- A tuner type is a lookup, a scan is a filter, and a source is the user's
- A channel is a real item, a series timer keeps its name, and a refused body is a 400
- A guide refresh owns a channel's lineup fields, not its item
- An airing is a real item, and a keyless row is not a group
- A keyless by-name row is item loss, not a grouping detail
- A channel no provider backs is 400, and four labels were wrong
- The Startup guard row asserts the guard, not a payload identity
- Repair the series-timer fan-out probe and stop three reds rendering as settled
- Port CreateItemByName, cumulative runtime ticks and the instant-mix dispatch
- Resolve instant-mix genre names through CreateItemByName, and restrict by-name counts to their ItemValue type
- Resolve LibraryOptions by parsed Uuid, not by id spelling
- Write JPEG the way Skia does, and stop advertising ranges on trickplay tiles
- Invalidate the image cache after the JPEG chroma change
- Port the RemoteSearch "Identify" cascade faithfully
- Pin a padded provider id the way int.Parse does
- Route POST /Library/Refresh through the task registry
- Port the group-state SessionJoined/SessionLeaving hooks; add a two-server push differential
- Port WaitingGroupState's third ResumePlaying arm; make the push probe report what it drops
- Port MusicArtistResolver; RecursiveItemCount; the remote-search fetcher gate
- Port the remote-search provider ordering; loose artist audio
- Port the per-state playback arms; drive them over the push differential
- Port the WaitingGroupState fallback; widen the push probe to every state
- A fresh play queue has nothing playing, not item 0
- Port the slug branch, per-CleanName counts and the Person row columns
- Port the PresentationUniqueKey collapse; guard ForcedSortName
- A missing traceId must diff red, and drop the dead Year guard
- Close the paths that let a wrong number look like a measurement
- A side-path note never settles the row it hangs off
- Filter LibraryChanged by what the user may actually see
- Derive LibraryChanged CollectionFolders the way GetTopParentIds does
- Stop delete_reports_a_failed_unlink failing when CI runs as root

### Documentation
- Record that Live TV channels 404 on GET /Items/{itemId}
- Make the body-binder claim carry its measurement
- Name the two unported IsMetadataFetcherEnabled arms

### Features
- A tuner is a kind, and HDHomeRun is one of them
- Port LibraryChangedNotifier so item writes announce themselves

### Miscellaneous
- Record the v0.40.0 parity+perf run

### Security
- Join the wrapped warning string in ensure_startup_wizard_integrity

### Testing
- Cover the user-root branch's IsVisible arm
- Probe the external-change webhooks; stop dropping ProviderIds on an item edit
- Regenerate the ledger — push layer verifies 20/20

## [0.40.0] - 2026-08-29

### Bug Fixes
- Write user data under the keys Jellyfin actually reads
- Key the Peoples cover index on a collation, not on LOWER()
- Anchor an interval on the last run, not on process start
- Serve NetworkConfiguration under the contract's field names
- Resolve jellyfin's virtual libraries, and find by-name rows by name and kind
- Confine an unscoped query to the user's libraries, and give created items a container
- Read an adopted Jellyfin database the way Jellyfin reads it
- Stop folding eth and thorn, and keep the keys we cannot rebuild
- Confine the counts and the by-name tabs to the user's libraries too
- Confine the filter facets and /Years to the user's libraries
- Resolve a UserView the way GetTopParentIdsForQuery does
- Make the perf gate say what is actually wrong
- Give a provisioned container the shape Jellyfin will recognise
- A grouping view stands for the libraries grouped into it
- Resume surfaces the version that was played, not the primary
- Reduce memory usage of dev builds

### Features
- Import jellyfin's config xml when adopting a data directory
- Enforce the network policy instead of only storing it
- Import network.xml when adopting a Jellyfin data directory
- Import Jellyfin's tuners and guide on adoption
- Resolve the real client address behind a known proxy

### Miscellaneous
- Record the ferrofin-core -> ferrofin-networking edge in the lockfile

### Testing
- Cover KnownProxies and the forwarded-header walk

## [0.39.1] - 2026-08-27

### Bug Fixes
- Scope Merge Versions' movie merging to one library

## [0.39.0] - 2026-08-27

### Bug Fixes
- Stop the benchmark measuring Jellyfin's startup stub

### Documentation
- Scope hardware transcoding to NVENC, VAAPI and QSV

### Features
- Probe the full ffmpeg capability surface
- Build the hardware device graphs and pick hardware encoders
- Choose the hardware decoder and assemble the input line
- Port the tonemapping paths and the shared filter fragments
- Port the software filter chain and the filtergraph assembly
- Port the NVENC/CUDA filter chain
- Drive NVENC from the ported hardware matrix
- Probe the VAAPI render node for its driver and Vulkan interop
- Port the Intel iHD and limited VAAPI filter chains
- Complete VAAPI -- AMD Vulkan chain, low-power params, wiring
- Port the QSV gate and the Linux QSV filter chain
- Complete QSV -- D3D11 chain, bitrate arms, wiring
- Strip Dolby Vision and HDR10+ metadata on a stream copy
- Report a starting transcode, and what hardware it runs on
- Port the accelerated trickplay path, and fix -vsync on ffmpeg 8+
- Run trickplay extraction on the GPU

### Miscellaneous
- Regenerate the parity ledger and record the latest benchmark runs

### Performance
- Drop the redundant DISTINCT from the by-name item count
- Skip the browse row-count when the page already proves it

### Testing
- Stop the VAAPI device goldens depending on the test machine's GPU
- The real-Jellyfin drop-in adoption harness

## [0.38.0] - 2026-08-24

### Testing
- Stop the restart tests failing on a busy machine

## [0.37.1] - 2026-08-23

### Bug Fixes
- Make scrubbing previews reachable by clients; honour the per-library gate
- Stopping a scheduled task kills its ffmpeg child
- Persist SortName on every write, and port CreateSortName faithfully
- Gate POST /System/Restart behind local-access-or-elevation

### Performance
- Default the scan probe window to 8
- Stop asking folders and by-name items for media sources
- Take the playback-metrics writes off the request path

## [0.37.0] - 2026-08-23

### Bug Fixes
- Derive Person ids from the case-normalized by-name key

### Documentation
- Retire the "deferred" classifications after the no-deferral sweep

### Features
- Port GetLocalizedString and re-stamp MediaStream.Localized* on read
- Index external subtitle/audio sidecars at scan time

### Performance
- Stop memsetting 128 KiB per pushed WebSocket message
- Stop re-decoding unchanged images on every rescan

## [0.36.0] - 2026-08-23

### Bug Fixes
- GroupingOptions lists only grouping-eligible libraries
- Subtitle writers match Jellyfin byte for byte
- Media sources carry their attachments and the N-form id
- Write guids and dates the way Jellyfin's JSON converters do
- Accept the contract's videoBitRate/audioBitRate spellings on HLS routes
- Port GetLatestItems — one query, grouped by index container
- Persist the whole user policy; classify guide programmes
- Stamp DateCreated/DateModified the way Jellyfin's resolver does
- The liked threshold is 6.5, as UserItemData.MinLikeValue says
- Hand the trickplay manager to the HLS chain
- Match the segment container case-insensitively
- Release the tuner when a transcode job dies
- Release a tuner once per viewer, and only when the client is gone
- Keep LastExecutionResult, and never lose a startup run
- Build by-name DtoOptions from the request, as Jellyfin does
- Give /Similar Jellyfin's phase-2 filter, seed guard and artist exclusion

### Features
- Port HlsCodecStringHelpers — the RFC 6381 CODECS strings
- Port the tvshows/music grouped-threshold latest query
- Port DynamicHlsHelper — the real master.m3u8
- Project channels through the DTO service, as Jellyfin does
- Project programmes through the DTO service, as Jellyfin does
- The "Refresh Guide" scheduled task
- Serve the live.m3u8 ffmpeg writes, as Jellyfin does
- The real live-stream flow — open a tuner once, share it, close it
- The DVR — timers that fire, recordings that play while recording
- Transcode the live stream a client opened, not the channel again
- The "Update Plugins" scheduled task
- The "Refresh Channels" scheduled task
- The core "Media Segment Scan" task, always registered
- Port the remaining Identify / remote-image / lyric paths
- Proxy the Schedules Direct country list as Jellyfin does
- Materialize the UserRootFolder, Year items and by-name collages

### Performance
- Index the SortName ordering, and give it Jellyfin's tiebreaker

### Refactor
- One CreateSortName port for the scanner and the guide

### Testing
- Pin the canonical-id close and the sort-key re-derivation

### Parity
- Bring the misclassified not-testable ops under real test
- Verify the host-side effects — password reset, backup, restart
- Terminal phase — restore, restart, shutdown observed live
- Fixtures and provisioning for music, subtitles, trickplay, Live TV
- Verify subtitle delete and media attachments
- Stream-signature layer for direct play, HLS, subtitles, trickplay
- Live TV journey — live streams, timers, in-progress recordings
- Opt-in remote-subtitle journey through OpenSubtitles
- Regenerate the ledger with the new layers live

### Style
- Rustfmt the live-stream planner test

## [0.35.0] - 2026-08-23

### Bug Fixes
- Project media folders without a user, as Jellyfin does
- Honour the client's fields on playlist items
- Stop exposing live access tokens in device listings
- Never gate local sidecar artwork on the ImageFetchers list
- Serve UserViews with Jellyfin's full default field set

### CI/CD
- Run the coverage gate before the Rust toolchain

### Features
- Shape honesty gate v2 — self-baseline excludes, Jellyfin diff publishes

### Performance
- Event-driven segment waits; gate the fMP4 init on segment 0

### Testing
- Assert the local-artwork regression via the repository seam

## [0.34.0] - 2026-08-22

### Bug Fixes
- Stop re-provisioning the Playlists folder on every read

### CI/CD
- Enforce the benchmark-coverage gate

### Features
- Add the extension kill-switch the benchmark assumed existed

### Performance
- Compress at ASP.NET's level instead of the codec's default
- Cut /Shows/NextUp CPU 6.5x — it was CPU-bound, not pool-bound

## [0.33.3] - 2026-08-22

### Bug Fixes
- Correct provider parity defects found in review
- One library read per similar-items request, and keep remotes out of recommendations
- Correct EXIF, NFO and EPUB edge cases found in review
- Close the remaining provider-parity gaps found in review
- Resolve remote similar-item batches the way C# does
- Persist the ids an NFO pins, and cap archive reads on bytes read
- Complete the photo DTO and keep EXIF reads off async workers
- Match upstream on book, playlist and OMDb lookup details
- Resolve items by their recorded ids, and correct three misported details
- Keep the presentation key in step with a merged version group
- Hoist test const above statements for clippy

### Documentation
- State plainly that the container-XML and playlist-file ports are unwired

### Miscellaneous
- Cleanup

### Performance
- Scope the episode text read to the items the scan planned
- Cache OMDb season listings, and finish the EXIF port
- Only compress responses larger than one TCP segment

## [0.33.2] - 2026-08-21

### Bug Fixes
- Default fsGroup 1000 so volume ownership self-heals on mount
- Gate the rest of the RequiresElevation surface

## [0.33.1] - 2026-08-21

### Bug Fixes
- Take backup work off the runtime, gate it, and serialize it

### Performance
- Stop reading the item row to serve an image that exists
- Probe fpcalc concurrently instead of on the critical path
- Resolve a page's cast without materializing 72-column rows
- Keep exporter-only span fields out of the no-export path
- Read the locked-item set once per scan, not once per item

## [0.33.0] - 2026-08-20

### Bug Fixes
- Match C# ordering and null semantics in similar items
- Translate a cast play request before pushing it
- Enforce the SyncPlay access policy on every route
- Gate SyncPlay groups on each member's library access
- Expand linked-child containers, and drop the SyncPlay list N+1
- Halve the reads in play translation, dedup, log denials
- Never re-derive an item id from the row in play translation
- Enforce Jellyfin's RequiresElevation policy

### Documentation
- Record how to profile the push paths, and what fails here

### Testing
- Make the push surface a first-class suite stage

### Style
- Formatting after the main merge

## [0.32.0] - 2026-08-20

### Bug Fixes
- Give episodes their title and synopsis from TMDB
- An episode's Cast & Crew comes back from TMDB
- Let a completed credits fetch clear a stale cast
- Make a failed frame extraction say why
- A failed credits request is not an empty cast
- Close the same credits hazard on the TVDB arm
- Gate episode re-fetches on the stored row, not the planned one
- Let TheTVDB report an episode miss, so TMDB gets a turn
- Give up when every chapter extraction is failing
- Only a real extraction resets the chapter failure streak
- An image already on disk is not proof extraction works
- A video that extracted before failing is both things
- Parent photos to their own directory's album
- Date folder-named audiobooks, correct books scan comments
- Skip cue sheets and emit an empty book SeriesName
- Resolve XML entities in XMLTV attribute values
- Bound two collections that grew for the life of the process
- Close three read-then-write races on the user and session paths

### Features
- External id descriptors and the item Links row
- Full OMDb metadata, image and search provider
- TMDB box-set metadata, artwork and identify
- Scan photos and read their embedded EXIF
- Album.nfo and artist.nfo readers and savers
- Remote similarity providers (TMDB similar, ListenBrainz)
- Scan books and read their embedded comic/EPUB metadata
- Collection/playlist XML, playlist files, MusicBrainz depth
- Scan books libraries into Book and AudioBook items

### Miscellaneous
- Drop dead cargo features and correct provider docs

### Performance
- Seek folder leaf counts from the ancestor closure

### Revert
- Drop the chapter failure-streak guard

## [0.31.0] - 2026-08-20

### Bug Fixes
- Actually apply the program query filters in the manager
- Stop degrading unparseable ids to the nil GUID
- Rank search results in SQL, before the LIMIT
- Scope socket unregistration to the socket that closed
- End a session when its last socket closes

### Features
- Compress responses, as Jellyfin does

### Performance
- Let jemalloc return memory while the server is idle
- Pipeline ffprobe during the library scan
- Trim the scheduled-task scheduler's per-tick work
- Skip the backtracking engine when a pattern provably cannot match

## [0.30.3] - 2026-08-20

### Bug Fixes
- Close five races that real parallelism made reachable
- Restore transcode keep-alives, honour SegmentLength, bound the log read

### Documentation
- Record what the fast loop cannot measure

### Performance
- Stop ORDER BY RANDOM() serializing on SQLite's global PRNG mutex
- Cut cold start 3.1x by overlapping the external probes
- Skip the user-data push read when nothing can receive it
- Return the server configuration by Arc instead of by deep clone

## [0.30.2] - 2026-08-20

### Bug Fixes
- Benchmark suite updates
- Stop holding the session lock across WebSocket sends
- Authenticate the hls1 segment routes, matching Jellyfin
- Scope /Items/Filters2 by parent regardless of `recursive`
- Stop slicing branding CSS with an index from its lowercased copy
- Log the config read failures that rendered as "empty"
- Make the two culture lists agree on their ordering
- Keep an unset display preference null instead of ""
- Apply the Live TV program query filters
- Restore the SQL-boundary ratchet

### Miscellaneous
- Refresh ledger from a full sweep — 191/412 deep-verified
- Drop the last of the old project name

### Performance
- Move prefetched relation rows into DTOs instead of cloning
- Move an item's stream rows into the DTO at their last read
- Move a stream row's text fields into the DTO instead of cloning
- Collapse the NextUp episode projection into one query
- Collapse the playlist-items read path
- Push LIMIT/OFFSET into the /Persons query
- Stop re-parsing and re-cloning on the authenticated request path
- Bind borrowed names in the by-name count queries
- Build the localization tables once
- Stop recomputing DTO lookup keys per name per item
- Stop buying the /Persons total with a second full pass
- Stop blocking a tokio worker per image row
- Remove SQLite's two global lock bottlenecks
- Request SQLite's maximum mmap ceiling, and pin why it needs no tuning
- Batch the /Devices listing out of its N+1
- Batch the per-item user-data reads behind /Items/Latest
- Force the join order on the suggestions aggregates

### Refactor
- Store the server configuration behind an Arc
- Rename the Ferrofin-owned schema objects off the old project name

### Testing
- Cover the legacy-authorization gate
- Pin the person-name key convention on both sides
- Add suite/micro — a ~10s measurement loop

## [0.30.1] - 2026-08-18

### Bug Fixes
- Address review findings — empty-page total, cycle guard, chunking
- Bump library_manager SQL boundary ceiling for cycle test

### Miscellaneous
- Update performance gate baseline

### Performance
- Jemalloc + batch queries for saturation-family endpoints

## [0.30.0] - 2026-08-17

### Bug Fixes
- An episode's Cast & Crew is the episode's, not the series'
- An episode's Cast & Crew is the episode's, not the series'
- Benchmark v2 — address review round 1 (blockers, ratio floor, pooling)
- Benchmark v2 — address review round 2 (viewer subline, warmup call floor, ratio-floor tests)
- Benchmark v2 — address review round 3 (vacuous test 11, ratio accounting totality)
- Honor FERROFIN/JELLYFIN_HOST_PORT in the perf legs
- Per-checkout compile-cache scope (CACHE_SCOPE) — shared cargo cache mounts cross-poisoned checkouts
- Idempotent provisioning + ctx files exempt from the raw wipe
- Scope the bench image tag like the compile cache
- Cap calibrated rates at BENCH_RATE_MAX (2000/s)
- Login storm runs LAST + failure taxonomy on partial rows
- Graceful stop for cold-leg restarts — compose restart's 10s grace SIGKILLed Jellyfin into an unbootable DB
- Cold probes wait out Jellyfin's post-restart 503 window, record it as ready_wait_ms
- 0.5 review round 1 — sidecar escape, load-time name guard, caps become settings
- 0.5 review round 2 — close the five low residuals

### Build
- Profiling profile — release speed with full debug info

### Documentation
- Profiling instructions — from benchmark row to root cause
- Perf_event_mlock_kb sysctl for samply on many-threaded servers
- Make the fetcher-enforcement upgrade note survive changelog regen

### Features
- Benchmark v2 phase A+B — fail-loud manifest + verified binary identity
- Benchmark v2 phases I+G — single-language Python suite on vegeta, open-loop comparison legs
- Benchmark v2 phase H — warm/cold protocol, cold as a first-class metric
- Benchmark v2 phase E — core-vs-extension ownership, machine-readable
- Benchmark v2 phase D — the noise floor is a first-class tie
- Benchmark v2 phase C — publishable records are N-run distributions
- Benchmark v2 phase F+G2 — fairness polish + the saturation knee
- Scripted rate calibration — suite/run.sh calibrate
- G4 — operator-tunable per-plugin state cap
- G2 — the scoped write family (WIT 0.5.0)
- G3 — embedded subtitle extraction
- G1.1 — richer metadata-result (supplement-only, entity-backed)
- G1.2 — named providers surface in library options
- Remote-images (G1.3) + real per-library fetcher gating/ordering

### Performance
- Sample-count windows + scan reuse — publish drops from ~18-21h to ~4-6h
- Drive user-data filters through the BaseItems PK, not a correlated EXISTS
- /Persons dedup via one covering-index aggregate pass + SQL paging

### Testing
- Cover the 0.5 settings builders + provider-name guard

### Results
- First open-loop v2 run record (run-2e894f3)

## [0.29.0] - 2026-08-16

### Bug Fixes
- Analysis-review fixes — drain deadlock, decode timeout, reserved keys
- Analysis-review round 2 — the doc edits that never landed, and four real fixes

### Documentation
- Analysis-review round 3 — the four small landings

### Features
- ABI 0.4.0 — the generic media-analysis capability

### Style
- Hoist the PermissionsExt import (items-after-statements)

## [0.28.1] - 2026-08-16

### Bug Fixes
- Run index.html transforms on the bare /web/ directory request

## [0.28.0] - 2026-08-16

### Bug Fixes
- Fingerprint with ffmpeg's chromaprint muxer

### Features
- Rolling updates by default

## [0.27.0] - 2026-08-16

### Bug Fixes
- Resolve a person filmography by item id, not just Peoples row id
- Capability-review round 1 — token leak closed, coverage restored

### Features
- WIT 0.3.0 — plugin routes, web transforms, rich queries, KV state
- 0.3.0 finishing pass — transform proof, wasm-hello demos, docs
- Declared egress — plugin-shipped public-network allowlist

### Testing
- Dedupe test Config via Config::test_stub
- 0.3.0 capability proof — KV caps, plugin routes end-to-end, uninstall state cleanup

## [0.26.0] - 2026-08-14

### Bug Fixes
- Harden repository install per review
- Close guid-squatting + provenance gaps from review round 2
- Round-3 review — ledger at the source, file-drop squat door
- Cap + time out repository fetches (round-4 review)
- Settings-page hardening from review round 5
- Round-8 minimal — id rule holds within the incoming batch

### Features
- Jellyfin-style repository install for WASM plugins
- Plugin settings pages — config-pages export + synthesized fallback (ABI 0.2.0)
- Round-7 hardening — disable disarms pages, immutable versions, DNS pin

## [0.25.2] - 2026-08-14

### Bug Fixes
- Make credits detection survive a small /tmp, and visible when it fails

## [0.25.1] - 2026-08-14

### Bug Fixes
- Cast device list, and chapter images after a scan

## [0.25.0] - 2026-08-13

### Bug Fixes
- Harden the plugin host per external review
- Untrack the perf-fixtures symlink, widen the private-IP deny, tidy the message
- Deny http-fetch during load; cache the event enabled-flag
- Serve chapter thumbnails — DTO tag + image route
- Library tiles sample the right kinds, after artwork lands

### CI/CD
- Build the WASM example guest and gate ferrofin-wasm coverage

### Documentation
- Two-tier plugin architecture, sandbox security first
- WASM plugin tier — sandbox model, authoring, knobs
- The WASM capability surface after E2
- Sweep the plugin story across CLAUDE.md, ARCHITECTURE, FEATURES, CONFIG, README
- Spell out the WASM memory ceiling semantics and real cost
- Confirm the 128 MiB per-plugin memory default

### Features
- Add the Tier-1b WASM plugin host (ferrofin-wasm)
- Load WASM plugins at the composition root
- Add the wasm-hello reference guest (toolchain island)
- E2 capabilities — http-fetch, query-items, write-media-segments
- E3 — WASM plugins as scan metadata sources (metadata-lookup)
- Deny private/loopback http-fetch by default, per-plugin allowlist
- Log session start/end and lockouts to the activity feed

### Refactor
- Fold provider-id merging into the dynamic metadata helper

### Testing
- Server-level HTTP test for WASM plugins; share the WAT fixture

## [0.24.1] - 2026-08-13

### Bug Fixes
- Episodes sort by number, not title — restores the play queue

## [0.24.0] - 2026-08-13

### Bug Fixes
- Kill the spinner class — index merged-version lookups, drop unknown enum tokens
- Episode counts exclude merged alternate versions
- Episode playback returns — startItemId id-casing, 3000x faster user-data sort, merged-version sources
- Honor the folder year when matching a series on TVDB
- Don't emit path-keyed MusicArtist rows

### Documentation
- Mark Live TV + DVR as not yet human-verified end-to-end

### Features
- Enforce the CollectionManagement policy on collection routes
- Populate the dashboard activity log with system events
- Download studio thumbs during the scan
- Persist and serve RemoteTrailers
- Album artwork from embedded cover art
- Artist → album → track hierarchy in the music scan
- Detect disc structures and record VideoType

## [0.23.0] - 2026-08-13

### Bug Fixes
- Person favorites round-trip — one Person item per name, Jellyfin-derived ids
- User-data sorts order for real — DatePlayed, PlayCount, Release fallback
- Date Added and Parental Rating sorts get real data at scan time
- Audio DTOs carry AlbumId; performer links prefer the browsable artist id

### Features
- The movie scan resolves extras into owned rows

## [0.22.0] - 2026-08-13

### Features
- Library tiles get real artwork — post-scan CollectionFolder collage

## [0.21.2] - 2026-08-13

### Bug Fixes
- FMP4 init serving never cancels the transcode start — fatal fragParsingError regression

## [0.21.1] - 2026-08-13

### Bug Fixes
- Filter facets reach Jellyfin coverage — provider merging, clean-value dedup, edit re-indexing
- Next Up returns — playback start stamps LastPlayedDate

## [0.21.0] - 2026-08-13

### Bug Fixes
- Collections become visible — BoxSet browses re-root onto linked-child ancestors
- Music libraries populate — scoped post-add scan, tag-derived track names

### Features
- Transcode and playback logs name the media, not just ids

## [0.20.1] - 2026-08-13

### Bug Fixes
- Jellyfin-web parity sweep — filters, favorites, latest rows, legacy routes, DTO gaps
- Transcode jobs die with their consumers; segment retries wait instead of killing
- Provider episode titles replace the filename placeholder; SeriesName comes from the series row
- Extract video frames into the temp dir, not beside the media

### Miscellaneous
- Fix cargo fmt issue

### Testing
- Cover the parity-sweep and transcode-lifecycle fixes

## [0.20.0] - 2026-08-13

### Bug Fixes
- User metadata edits survive library scans (honor IsLocked)
- Uploaded images survive the scan's artwork rewrite
- Scope the episode merge key to the series row, not its name

### Features
- Log why a websocket session is anonymous

### Miscellaneous
- Remove legacy implementation-plan docs

## [0.19.4] - 2026-08-12

### Bug Fixes
- Stop the library scan from clobbering columns it does not own
- Merge/split write their link column instead of full stale rows

### CI/CD
- Free runner disk and strip debuginfo so jobs fit GitHub-hosted runners
- Source homelab infra endpoints from CI/CD variables

### Miscellaneous
- Stop tracking .claude/ and .mcp.json (local agent tooling)

## [0.19.3] - 2026-08-12

### CI/CD
- Make base-image builds resilient to github.com flakiness

### Documentation
- Phase 5 — README front door, feature matrix, extensions, config
- Note the pre-publication history rewrite

### Refactor
- Rename Hermit → Ferrofin across the codebase

## [0.19.2] - 2026-08-12

### CI/CD
- Skip unused Dockerfile stages under kaniko (fix release web rebuild)

### Testing
- Allowlist PlaySessionId in the no-lowercase-GUID invariant

## [0.19.1] - 2026-08-12

### Bug Fixes
- Stop the 0007 rebuild from cascade-deleting all user data
- Complete 0007's GUID-uppercase coverage (audit follow-up)

### Testing
- Verify + classify the 8 flagged ledger rows
- Unit-test the extension crates and gate their coverage

## [0.19.0] - 2026-08-12

### Bug Fixes
- Give episodes their cast — merge series regulars + surface roles
- Detect BOM-less UTF-16 + restore the -sub_charenc hint

### Documentation
- Drop stale PORT_REPORT.md reference from contract-superset doc
- Promote the knowledge base's load-bearing docs into public docs/

### Features
- Store GUIDs and datetimes in Jellyfin's exact text formats
- Pin the schema to Jellyfin 10.11.8 — migration 0007 + code convergence
- BaseItems.Data JSON is the playlist/collection source of truth
- Adopt an existing Jellyfin 10.11.8 database in place

### Miscellaneous
- Release hygiene — community files, self-contained builds, de-identification

### Testing
- Schema-conformance gate + the drop-in round-trip test

## [0.18.0] - 2026-08-10

### Features
- Live filesystem watching of library roots (inotify via notify)
- Debounce filesystem changes behind LibraryMonitorDelay
- Path-scoped ingest — resolve just the changed files

## [0.17.0] - 2026-08-10

### Bug Fixes
- Report CanUninstall=true so the dashboard shows the enable/disable toggle
- Correct transcode.js fixture path after the suite/ reorg

### Features
- Accumulate benchmark reruns per SHA instead of overwriting
- Prune items whose files were deleted from disk
- Push UserDataChanged to the user's other devices
- Send the WebSocket pushes Jellyfin clients rely on

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
- Clean release record v0.12.0 / de2dc00 (parity + perf)

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
- Add HERMIT_ENABLE_METRICS bootstrap override

### Miscellaneous
- Add run-benchmark skill
- Add missing crate dependency
- Add secrecy crate dependency to hermit-providers and hermit-model

### Performance
- Serve user DTOs from the auth cache instead of 2-3 DB round-trips per request
- Reuse a pre-minted token for post-load captures

### Refactor
- Give subsystem crates typed errors that convert into ServiceError

### Bench
- Extend the surface to write paths — 4 POST variants + write-row comparability
- Release suite record 6d6f32f (parity + perf)

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
- Drop the stale 0157db4 entry from runs.json too
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
- Add Hermit-only p50/p95/p99 regression gate (plan 4)
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
- Classify playstate-progress + playlist-share-delete as methodology (not Hermit bugs)

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
- Fix env example to HERMIT_LOG (the var Hermit reads)
- Triage roadmap + per-op verdicts from the parity-triage workflow

### Features
- Chart-managed env ConfigMap injected into Hermit

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
- Bundle pinned jellyfin-web client at /usr/share/hermit/web

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
- Scaffold hermit workspace + Wave 0 port
- Port MediaBrowser.Model DTOs and enums
- Wave 2
- Port Jellyfin.Database schema + entities (sqlx + SQLite)
- Port MediaBrowser.Controller interfaces (the DI seam)
- Wave 5 impl crates
- Port core manager implementations (the workhorse)
- Port Jellyfin.Api — contract-complete axum layer
- Implement 236 endpoints for real (Wave 7b)
- Real HLS transcode pipeline + more endpoints
- Composition root — Hermit boots as a real server (Wave 8)
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
- Plan + hermit-chromaprint pure intro/credits math
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
- Working Hermit-vs-Jellyfin harness, first run green
- The full Jellyfin dashboard task set + a real trigger scheduler
- Real transcode TTFS — copy + forced-encode modes; fix 30s transcode start timeout
- Honor the negotiated bitrate/resolution caps — downscale, -maxrate, HDR tonemap
- Record playback decisions to PlaybackSessions (Track A)
- Bus-registered sockets are remote-controllable

### Miscellaneous
- Gitignore .rcg/
- Lockfile for hermit-networking dependency
- Fake MediaSourceManager impls + Cargo.lock for refresh_media_streams
- Refresh Cargo.lock for hermit-providers tokio/tracing deps
- Add hermitcodegraph server entry

### Performance
- Halve time-to-first-segment (temp_file + drop index+1 wait)
- Small SQLite pool + batched DTO relation loads
- 3s HLS segments + forced segment-boundary keyframes

### Style
- Rustfmt the person_ids filmography test
- Rustfmt the extensions registered_plugins builder


